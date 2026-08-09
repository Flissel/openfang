//! Runtime admission coordination between the kernel and persistent authority state.

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use openfang_memory::runtime_authority::{RuntimeAuthorityStore, StoreAdmission};
use openfang_types::runtime_admission::{
    ExecutionReceiptV1, ReceiptFinalizationV1, RuntimeAdmissionContextV1,
    RuntimeAdmissionDecisionV1, RuntimeCostReservationOutcomeV1, RuntimeCostReservationRequestV1,
    RuntimePlanApprovalOutcomeV1, RuntimePlanApprovalRequestV1,
};
use std::sync::Arc;

/// Clock abstraction so admission timestamps are deterministic in tests.
pub trait RuntimeClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Production clock for runtime-admission persistence.
#[derive(Default)]
pub struct SystemRuntimeClock;

impl RuntimeClock for SystemRuntimeClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Authority operations required by the runtime-admission coordinator.
pub trait RuntimeAuthorityRepository: Send + Sync {
    fn request(
        &self,
        request: RuntimePlanApprovalRequestV1,
        auto_approve: bool,
        now: DateTime<Utc>,
    ) -> Result<RuntimePlanApprovalOutcomeV1, String>;
    fn resolve(&self, approval_ref: &str, approved: bool, now: DateTime<Utc>)
        -> Result<(), String>;
    fn reserve(
        &self,
        request: RuntimeCostReservationRequestV1,
        now: DateTime<Utc>,
    ) -> Result<RuntimeCostReservationOutcomeV1, String>;
    fn admit(
        &self,
        context: RuntimeAdmissionContextV1,
        now: DateTime<Utc>,
    ) -> Result<StoreAdmission, String>;
    fn mark_dispatching(
        &self,
        invocation_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ExecutionReceiptV1, String>;
    fn finalize(
        &self,
        invocation_id: &str,
        update: ReceiptFinalizationV1,
        now: DateTime<Utc>,
    ) -> Result<ExecutionReceiptV1, String>;
    fn mark_outcome_unknown(
        &self,
        invocation_id: &str,
        error_class: &str,
        now: DateTime<Utc>,
    ) -> Result<ExecutionReceiptV1, String>;
    fn get_receipt(&self, invocation_id: &str) -> Result<Option<ExecutionReceiptV1>, String>;
    fn reconcile(&self, cutoff: DateTime<Utc>, now: DateTime<Utc>) -> Result<u64, String>;
}

impl RuntimeAuthorityRepository for RuntimeAuthorityStore {
    fn request(
        &self,
        request: RuntimePlanApprovalRequestV1,
        auto_approve: bool,
        now: DateTime<Utc>,
    ) -> Result<RuntimePlanApprovalOutcomeV1, String> {
        self.request_plan_approval(request, auto_approve, now)
            .map_err(|error| error.to_string())
    }

    fn resolve(
        &self,
        approval_ref: &str,
        approved: bool,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        self.resolve_plan_approval(approval_ref, approved, now)
            .map_err(|error| error.to_string())
    }

    fn reserve(
        &self,
        request: RuntimeCostReservationRequestV1,
        now: DateTime<Utc>,
    ) -> Result<RuntimeCostReservationOutcomeV1, String> {
        self.reserve_cost(request, now)
            .map_err(|error| error.to_string())
    }

    fn admit(
        &self,
        context: RuntimeAdmissionContextV1,
        now: DateTime<Utc>,
    ) -> Result<StoreAdmission, String> {
        RuntimeAuthorityStore::admit(self, context, now).map_err(|error| error.to_string())
    }

    fn mark_dispatching(
        &self,
        invocation_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ExecutionReceiptV1, String> {
        RuntimeAuthorityStore::mark_dispatching(self, invocation_id, now)
            .map_err(|error| error.to_string())
    }

    fn finalize(
        &self,
        invocation_id: &str,
        update: ReceiptFinalizationV1,
        now: DateTime<Utc>,
    ) -> Result<ExecutionReceiptV1, String> {
        RuntimeAuthorityStore::finalize(self, invocation_id, update, now)
            .map_err(|error| error.to_string())
    }

    fn mark_outcome_unknown(
        &self,
        invocation_id: &str,
        error_class: &str,
        now: DateTime<Utc>,
    ) -> Result<ExecutionReceiptV1, String> {
        RuntimeAuthorityStore::mark_outcome_unknown(self, invocation_id, error_class, now)
            .map_err(|error| error.to_string())
    }

    fn get_receipt(&self, invocation_id: &str) -> Result<Option<ExecutionReceiptV1>, String> {
        RuntimeAuthorityStore::get_receipt(self, invocation_id).map_err(|error| error.to_string())
    }

    fn reconcile(&self, cutoff: DateTime<Utc>, now: DateTime<Utc>) -> Result<u64, String> {
        self.reconcile_stale_dispatching(cutoff, now)
            .map_err(|error| error.to_string())
    }
}

/// Coordinates admission decisions while the database remains the restart-safe authority.
pub struct RuntimeAdmissionService {
    store: Arc<dyn RuntimeAuthorityRepository>,
    clock: Arc<dyn RuntimeClock>,
    singleflight: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    receipt_max_bytes: usize,
    dispatch_stale_after_secs: u64,
    enabled: bool,
}

impl RuntimeAdmissionService {
    /// Return the single receipt retention bound validated at service construction.
    pub fn receipt_max_bytes(&self) -> usize {
        self.receipt_max_bytes
    }

    pub fn new(
        store: Arc<dyn RuntimeAuthorityRepository>,
        clock: Arc<dyn RuntimeClock>,
        receipt_max_bytes: usize,
        dispatch_stale_after_secs: u64,
    ) -> Result<Self, String> {
        Self::new_with_enabled(
            store,
            clock,
            receipt_max_bytes,
            dispatch_stale_after_secs,
            true,
        )
    }

    pub fn new_with_enabled(
        store: Arc<dyn RuntimeAuthorityRepository>,
        clock: Arc<dyn RuntimeClock>,
        receipt_max_bytes: usize,
        dispatch_stale_after_secs: u64,
        enabled: bool,
    ) -> Result<Self, String> {
        if !(1..=65_536).contains(&receipt_max_bytes) {
            return Err("receipt_max_bytes must be between 1 and 65536".into());
        }
        if dispatch_stale_after_secs == 0 {
            return Err("dispatch_stale_after_secs must be positive".into());
        }
        Ok(Self {
            store,
            clock,
            singleflight: DashMap::new(),
            receipt_max_bytes,
            dispatch_stale_after_secs,
            enabled,
        })
    }

    fn require_enabled(&self) -> Result<(), String> {
        if self.enabled {
            Ok(())
        } else {
            Err("runtime_admission_disabled".into())
        }
    }

    pub fn request_plan_approval(
        &self,
        request: RuntimePlanApprovalRequestV1,
        auto_approve: bool,
    ) -> Result<RuntimePlanApprovalOutcomeV1, String> {
        self.require_enabled()?;
        request.validate()?;
        let outcome = self
            .store
            .request(request, auto_approve, self.clock.now())?;
        outcome.validate()?;
        Ok(outcome)
    }

    pub fn resolve(&self, approval_ref: &str, approved: bool) -> Result<(), String> {
        self.require_enabled()?;
        self.store.resolve(approval_ref, approved, self.clock.now())
    }

    pub fn reserve(
        &self,
        request: RuntimeCostReservationRequestV1,
    ) -> Result<RuntimeCostReservationOutcomeV1, String> {
        self.require_enabled()?;
        request.validate()?;
        let outcome = self.store.reserve(request.clone(), self.clock.now())?;
        outcome.validate_against(&request)?;
        Ok(outcome)
    }

    pub async fn admit(
        &self,
        context: RuntimeAdmissionContextV1,
    ) -> Result<RuntimeAdmissionDecisionV1, String> {
        self.require_enabled()?;
        self.recover_stale_dispatches()?;
        context.validate()?;
        let invocation_id = context.invocation_id.clone();
        let lock = self
            .singleflight
            .entry(invocation_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let guard = lock.lock().await;
        let result = self
            .store
            .admit(context, self.clock.now())
            .map(map_store_admission)
            .and_then(|decision| {
                decision.validate(self.receipt_max_bytes)?;
                Ok(decision)
            });
        drop(guard);
        self.singleflight.remove_if(&invocation_id, |_, candidate| {
            Arc::ptr_eq(candidate, &lock) && Arc::strong_count(candidate) == 2
        });
        result
    }

    pub fn mark_dispatching(&self, invocation_id: &str) -> Result<ExecutionReceiptV1, String> {
        self.require_enabled()?;
        let receipt = self
            .store
            .mark_dispatching(invocation_id, self.clock.now())?;
        receipt.validate(self.receipt_max_bytes)?;
        Ok(receipt)
    }

    pub fn finalize(
        &self,
        invocation_id: &str,
        update: ReceiptFinalizationV1,
    ) -> Result<ExecutionReceiptV1, String> {
        self.require_enabled()?;
        update.validate(self.receipt_max_bytes)?;
        let receipt = self
            .store
            .finalize(invocation_id, update, self.clock.now())?;
        receipt.validate(self.receipt_max_bytes)?;
        Ok(receipt)
    }

    pub fn mark_outcome_unknown(
        &self,
        invocation_id: &str,
        error_class: String,
    ) -> Result<ExecutionReceiptV1, String> {
        self.require_enabled()?;
        let receipt =
            self.store
                .mark_outcome_unknown(invocation_id, &error_class, self.clock.now())?;
        receipt.validate(self.receipt_max_bytes)?;
        Ok(receipt)
    }

    pub fn read_receipt(&self, invocation_id: &str) -> Result<Option<ExecutionReceiptV1>, String> {
        let receipt = self.store.get_receipt(invocation_id)?;
        if let Some(receipt) = &receipt {
            receipt.validate(self.receipt_max_bytes)?;
        }
        Ok(receipt)
    }

    /// Reconcile durable in-flight dispatches during boot; errors are intentionally visible.
    pub fn recover_stale_dispatches(&self) -> Result<u64, String> {
        let now = self.clock.now();
        let duration = Duration::from_std(std::time::Duration::from_secs(
            self.dispatch_stale_after_secs,
        ))
        .map_err(|error| format!("invalid dispatch_stale_after_secs: {error}"))?;
        let cutoff = now.checked_sub_signed(duration).ok_or_else(|| {
            "dispatch_stale_after_secs produces an unrepresentable cutoff".to_string()
        })?;
        self.store.reconcile(cutoff, now)
    }

    /// A process restart cannot prove any old dispatch outcome, regardless of age.
    pub fn recover_after_restart(&self) -> Result<u64, String> {
        let now = self.clock.now();
        self.store.reconcile(now, now)
    }

    #[cfg(test)]
    fn singleflight_len(&self) -> usize {
        self.singleflight.len()
    }
}

fn map_store_admission(admission: StoreAdmission) -> RuntimeAdmissionDecisionV1 {
    match admission {
        StoreAdmission::Denied { reason_code } if reason_code.starts_with("cost_") => {
            RuntimeAdmissionDecisionV1::CostUnavailable { reason_code }
        }
        StoreAdmission::Denied { reason_code } => {
            RuntimeAdmissionDecisionV1::Denied { reason_code }
        }
        StoreAdmission::PendingApproval { approval_ref } => {
            RuntimeAdmissionDecisionV1::PendingApproval { approval_ref }
        }
        StoreAdmission::Admitted { receipt } => RuntimeAdmissionDecisionV1::Admitted { receipt },
        StoreAdmission::InProgress { receipt } => {
            RuntimeAdmissionDecisionV1::InProgress { receipt }
        }
        StoreAdmission::Replay { receipt } => RuntimeAdmissionDecisionV1::Replay { receipt },
        StoreAdmission::OutcomeUnknown { receipt } => {
            RuntimeAdmissionDecisionV1::OutcomeUnknown { receipt }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;
    use openfang_types::runtime_admission::{ExecutionReceiptStatus, ResultStorageMode};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Barrier, Condvar, Mutex};
    use std::time::Duration as StdDuration;

    const FIXED_TIME: &str = "2026-08-08T10:00:00Z";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<DateTime<Utc>>>);

    impl FakeClock {
        fn fixed() -> Self {
            Self(Arc::new(Mutex::new(now())))
        }

        fn advance(&self, seconds: i64) {
            let mut value = self.0.lock().expect("clock");
            *value = value
                .checked_add_signed(Duration::seconds(seconds))
                .expect("representable clock advance");
        }
    }

    impl RuntimeClock for FakeClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().expect("clock")
        }
    }

    struct FakeRuntimeAuthorityRepository {
        admissions: Mutex<VecDeque<StoreAdmission>>,
        receipt: Mutex<Option<ExecutionReceiptV1>>,
        recovery_count: u64,
        held_unknown: AtomicBool,
        approval_lookup_calls: AtomicUsize,
        cost_consumption_calls: AtomicUsize,
        dispatch_calls: AtomicUsize,
        receipt_write_calls: AtomicUsize,
        settlement_calls: AtomicUsize,
        protocol_mode: bool,
        overlap: (Mutex<usize>, Condvar),
        active_admit_calls: AtomicUsize,
        max_concurrent_admit_calls: AtomicUsize,
        reconcile_calls: AtomicUsize,
        reconcile_args: Mutex<Vec<(DateTime<Utc>, DateTime<Utc>)>>,
    }

    impl FakeRuntimeAuthorityRepository {
        fn with_admissions(admissions: Vec<StoreAdmission>) -> Arc<Self> {
            Arc::new(Self {
                admissions: Mutex::new(admissions.into()),
                receipt: Mutex::new(None),
                recovery_count: 0,
                held_unknown: AtomicBool::new(false),
                approval_lookup_calls: AtomicUsize::new(0),
                cost_consumption_calls: AtomicUsize::new(0),
                dispatch_calls: AtomicUsize::new(0),
                receipt_write_calls: AtomicUsize::new(0),
                settlement_calls: AtomicUsize::new(0),
                protocol_mode: false,
                overlap: (Mutex::new(0), Condvar::new()),
                active_admit_calls: AtomicUsize::new(0),
                max_concurrent_admit_calls: AtomicUsize::new(0),
                reconcile_calls: AtomicUsize::new(0),
                reconcile_args: Mutex::new(Vec::new()),
            })
        }

        fn protocol(receipt: Option<ExecutionReceiptV1>) -> Arc<Self> {
            Arc::new(Self {
                admissions: Mutex::new(VecDeque::new()),
                receipt: Mutex::new(receipt),
                recovery_count: 0,
                held_unknown: AtomicBool::new(false),
                approval_lookup_calls: AtomicUsize::new(0),
                cost_consumption_calls: AtomicUsize::new(0),
                dispatch_calls: AtomicUsize::new(0),
                receipt_write_calls: AtomicUsize::new(0),
                settlement_calls: AtomicUsize::new(0),
                protocol_mode: true,
                overlap: (Mutex::new(0), Condvar::new()),
                active_admit_calls: AtomicUsize::new(0),
                max_concurrent_admit_calls: AtomicUsize::new(0),
                reconcile_calls: AtomicUsize::new(0),
                reconcile_args: Mutex::new(Vec::new()),
            })
        }

        fn count(&self, counter: &AtomicUsize) -> usize {
            counter.load(Ordering::SeqCst)
        }

        fn invoke_dispatch(&self) {
            self.dispatch_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl RuntimeAuthorityRepository for FakeRuntimeAuthorityRepository {
        fn request(
            &self,
            _: RuntimePlanApprovalRequestV1,
            _: bool,
            _: DateTime<Utc>,
        ) -> Result<RuntimePlanApprovalOutcomeV1, String> {
            Err("unused".into())
        }
        fn resolve(&self, _: &str, _: bool, _: DateTime<Utc>) -> Result<(), String> {
            Err("unused".into())
        }
        fn reserve(
            &self,
            _: RuntimeCostReservationRequestV1,
            _: DateTime<Utc>,
        ) -> Result<RuntimeCostReservationOutcomeV1, String> {
            Err("unused".into())
        }
        fn admit(
            &self,
            context: RuntimeAdmissionContextV1,
            _: DateTime<Utc>,
        ) -> Result<StoreAdmission, String> {
            self.approval_lookup_calls.fetch_add(1, Ordering::SeqCst);
            if self.protocol_mode {
                let active = self.active_admit_calls.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_concurrent_admit_calls
                    .fetch_max(active, Ordering::SeqCst);
                let (gate, wake) = &self.overlap;
                let entered = gate.lock().map_err(|_| "poisoned overlap".to_string())?;
                let mut entered = entered;
                *entered += 1;
                if *entered == 1 {
                    let waited = wake
                        .wait_timeout_while(entered, StdDuration::from_millis(75), |count| {
                            *count < 2
                        })
                        .map_err(|_| "poisoned overlap".to_string())?;
                    entered = waited.0;
                } else {
                    wake.notify_all();
                }
                drop(entered);

                let outcome = {
                    let mut stored = self
                        .receipt
                        .lock()
                        .map_err(|_| "poisoned fake".to_string())?;
                    match stored.as_ref() {
                        None => {
                            let mut prepared = receipt(ExecutionReceiptStatus::Prepared);
                            prepared.invocation_id = context.invocation_id;
                            self.cost_consumption_calls.fetch_add(1, Ordering::SeqCst);
                            self.receipt_write_calls.fetch_add(1, Ordering::SeqCst);
                            *stored = Some(prepared.clone());
                            StoreAdmission::Admitted { receipt: prepared }
                        }
                        Some(existing)
                            if matches!(
                                existing.status,
                                ExecutionReceiptStatus::Prepared
                                    | ExecutionReceiptStatus::Dispatching
                            ) =>
                        {
                            StoreAdmission::InProgress {
                                receipt: existing.clone(),
                            }
                        }
                        Some(existing)
                            if matches!(
                                existing.status,
                                ExecutionReceiptStatus::Succeeded | ExecutionReceiptStatus::Failed
                            ) =>
                        {
                            StoreAdmission::Replay {
                                receipt: existing.clone(),
                            }
                        }
                        Some(existing) => StoreAdmission::OutcomeUnknown {
                            receipt: existing.clone(),
                        },
                    }
                };
                self.active_admit_calls.fetch_sub(1, Ordering::SeqCst);
                return Ok(outcome);
            }
            let admission = self
                .admissions
                .lock()
                .map_err(|_| "poisoned fake".to_string())?
                .pop_front()
                .ok_or_else(|| "missing admission".to_string())?;
            if matches!(admission, StoreAdmission::Admitted { .. }) {
                self.cost_consumption_calls.fetch_add(1, Ordering::SeqCst);
                self.receipt_write_calls.fetch_add(1, Ordering::SeqCst);
            }
            Ok(admission)
        }
        fn mark_dispatching(
            &self,
            invocation_id: &str,
            now: DateTime<Utc>,
        ) -> Result<ExecutionReceiptV1, String> {
            let mut stored = self
                .receipt
                .lock()
                .map_err(|_| "poisoned fake".to_string())?;
            let receipt = stored
                .as_mut()
                .ok_or_else(|| "missing receipt".to_string())?;
            if receipt.invocation_id != invocation_id
                || receipt.status != ExecutionReceiptStatus::Prepared
            {
                return Err("not prepared".into());
            }
            receipt.status = ExecutionReceiptStatus::Dispatching;
            receipt.dispatch_started_at = Some(now);
            Ok(receipt.clone())
        }
        fn finalize(
            &self,
            invocation_id: &str,
            update: ReceiptFinalizationV1,
            now: DateTime<Utc>,
        ) -> Result<ExecutionReceiptV1, String> {
            let mut stored = self
                .receipt
                .lock()
                .map_err(|_| "poisoned fake".to_string())?;
            let receipt = stored
                .as_mut()
                .ok_or_else(|| "missing receipt".to_string())?;
            if receipt.invocation_id != invocation_id
                || receipt.status != ExecutionReceiptStatus::Dispatching
            {
                return Err("not dispatching".into());
            }
            receipt.status = match update.status {
                openfang_types::runtime_admission::ReceiptFinalStatus::Succeeded => {
                    ExecutionReceiptStatus::Succeeded
                }
                openfang_types::runtime_admission::ReceiptFinalStatus::Failed => {
                    ExecutionReceiptStatus::Failed
                }
                openfang_types::runtime_admission::ReceiptFinalStatus::OutcomeUnknown => {
                    ExecutionReceiptStatus::OutcomeUnknown
                }
            };
            receipt.result_sha256 = update.result_sha256;
            receipt.result_storage_mode = update.result_storage_mode;
            receipt.result_envelope = update.result_envelope;
            receipt.error_class = update.error_class;
            receipt.finished_at = Some(now);
            self.settlement_calls.fetch_add(1, Ordering::SeqCst);
            Ok(receipt.clone())
        }
        fn mark_outcome_unknown(
            &self,
            _: &str,
            _: &str,
            _: DateTime<Utc>,
        ) -> Result<ExecutionReceiptV1, String> {
            Err("unused".into())
        }
        fn get_receipt(&self, _: &str) -> Result<Option<ExecutionReceiptV1>, String> {
            Ok(self
                .receipt
                .lock()
                .map_err(|_| "poisoned fake".to_string())?
                .clone())
        }
        fn reconcile(&self, cutoff: DateTime<Utc>, now: DateTime<Utc>) -> Result<u64, String> {
            self.reconcile_calls.fetch_add(1, Ordering::SeqCst);
            self.reconcile_args
                .lock()
                .map_err(|_| "poisoned reconcile".to_string())?
                .push((cutoff, now));
            let mut stored = self
                .receipt
                .lock()
                .map_err(|_| "poisoned fake".to_string())?;
            if let Some(receipt) = stored.as_mut() {
                if receipt.status == ExecutionReceiptStatus::Dispatching
                    && receipt
                        .dispatch_started_at
                        .is_some_and(|started| started <= cutoff)
                {
                    receipt.status = ExecutionReceiptStatus::OutcomeUnknown;
                    receipt.error_class = Some("stale_dispatching".into());
                    receipt.finished_at = Some(now);
                    self.held_unknown.store(true, Ordering::SeqCst);
                    return Ok(1);
                }
            }
            Ok(self.recovery_count)
        }
    }

    fn now() -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(FIXED_TIME, "%Y-%m-%dT%H:%M:%SZ")
            .expect("fixed timestamp")
            .and_utc()
    }

    fn service(repository: Arc<FakeRuntimeAuthorityRepository>) -> RuntimeAdmissionService {
        RuntimeAdmissionService::new(repository, Arc::new(FakeClock::fixed()), 1024, 60)
            .expect("service")
    }

    fn successful_finalization() -> ReceiptFinalizationV1 {
        ReceiptFinalizationV1 {
            status: openfang_types::runtime_admission::ReceiptFinalStatus::Succeeded,
            actual_cost_micro_usd: 1,
            result_sha256: Some(DIGEST.into()),
            result_storage_mode: Some(ResultStorageMode::RedactedEnvelope),
            result_envelope: Some("result".into()),
            error_class: None,
        }
    }

    fn context() -> RuntimeAdmissionContextV1 {
        RuntimeAdmissionContextV1 {
            invocation_id: "invocation-1".into(),
            caller_agent_id: "agent-1".into(),
            approval_ref: "approval-1".into(),
            cost_ref: "cost-1".into(),
            tool_name: "safe_tool".into(),
            arguments_sha256: DIGEST.into(),
        }
    }

    fn receipt(status: ExecutionReceiptStatus) -> ExecutionReceiptV1 {
        let started = if matches!(status, ExecutionReceiptStatus::Prepared) {
            None
        } else {
            Some(now())
        };
        let finished = if matches!(
            status,
            ExecutionReceiptStatus::Succeeded
                | ExecutionReceiptStatus::Failed
                | ExecutionReceiptStatus::OutcomeUnknown
        ) {
            Some(now())
        } else {
            None
        };
        ExecutionReceiptV1 {
            invocation_id: "invocation-1".into(),
            correlation_id: "correlation-1".into(),
            plan_id: "plan-1".into(),
            plan_revision: 1,
            space_id: "space-1".into(),
            agent_id: "agent-1".into(),
            tool_name: "safe_tool".into(),
            arguments_sha256: DIGEST.into(),
            approval_ref: "approval-1".into(),
            cost_ref: "cost-1".into(),
            status,
            result_sha256: matches!(status, ExecutionReceiptStatus::Succeeded)
                .then(|| DIGEST.into()),
            result_storage_mode: matches!(status, ExecutionReceiptStatus::Succeeded)
                .then_some(ResultStorageMode::RedactedEnvelope),
            result_envelope: matches!(status, ExecutionReceiptStatus::Succeeded)
                .then_some("result".into()),
            error_class: matches!(
                status,
                ExecutionReceiptStatus::Failed | ExecutionReceiptStatus::OutcomeUnknown
            )
            .then_some("test_error".into()),
            prepared_at: now(),
            dispatch_started_at: started,
            finished_at: finished,
        }
    }

    #[tokio::test]
    async fn denied_approval_stops_before_cost_consumption_and_dispatch() {
        let repository =
            FakeRuntimeAuthorityRepository::with_admissions(vec![StoreAdmission::Denied {
                reason_code: "approval_denied".into(),
            }]);
        let decision = service(repository.clone())
            .admit(context())
            .await
            .expect("decision");
        assert!(matches!(
            decision,
            RuntimeAdmissionDecisionV1::Denied { .. }
        ));
        assert_eq!(repository.count(&repository.approval_lookup_calls), 1);
        assert_eq!(repository.count(&repository.cost_consumption_calls), 0);
        assert_eq!(repository.count(&repository.dispatch_calls), 0);
        assert_eq!(repository.count(&repository.receipt_write_calls), 0);
        assert_eq!(repository.count(&repository.settlement_calls), 0);
    }

    #[tokio::test]
    async fn pending_approval_returns_only_approval_ref_without_cost_or_dispatch() {
        let repository = FakeRuntimeAuthorityRepository::with_admissions(vec![
            StoreAdmission::PendingApproval {
                approval_ref: "approval-pending".into(),
            },
        ]);
        let decision = service(repository.clone())
            .admit(context())
            .await
            .expect("decision");
        assert_eq!(
            decision,
            RuntimeAdmissionDecisionV1::PendingApproval {
                approval_ref: "approval-pending".into()
            }
        );
        assert_eq!(repository.count(&repository.cost_consumption_calls), 0);
        assert_eq!(repository.count(&repository.dispatch_calls), 0);
        assert_eq!(repository.count(&repository.receipt_write_calls), 0);
        assert_eq!(repository.count(&repository.settlement_calls), 0);
    }

    #[tokio::test]
    async fn mismatched_expired_or_consumed_cost_ref_stops_before_dispatch() {
        for reason_code in ["cost_binding_mismatch", "cost_expired", "cost_consumed"] {
            let repository =
                FakeRuntimeAuthorityRepository::with_admissions(vec![StoreAdmission::Denied {
                    reason_code: reason_code.into(),
                }]);
            let decision = service(repository.clone())
                .admit(context())
                .await
                .expect("decision");
            assert_eq!(
                decision,
                RuntimeAdmissionDecisionV1::CostUnavailable {
                    reason_code: reason_code.into()
                }
            );
            assert_eq!(repository.count(&repository.dispatch_calls), 0);
            assert_eq!(repository.count(&repository.settlement_calls), 0);
        }
    }

    #[tokio::test]
    async fn replay_returns_stored_receipt_without_redispatch_or_recharge() {
        let repository = FakeRuntimeAuthorityRepository::protocol(None);
        let admission_service = service(repository.clone());
        let admitted = admission_service.admit(context()).await.expect("first");
        let RuntimeAdmissionDecisionV1::Admitted { receipt } = admitted else {
            panic!("expected admitted")
        };
        admission_service
            .mark_dispatching(&receipt.invocation_id)
            .expect("dispatching");
        repository.invoke_dispatch();
        let succeeded = admission_service
            .finalize(&receipt.invocation_id, successful_finalization())
            .expect("finalized");
        assert_eq!(
            admission_service.admit(context()).await.expect("replay"),
            RuntimeAdmissionDecisionV1::Replay { receipt: succeeded }
        );
        assert_eq!(repository.count(&repository.cost_consumption_calls), 1);
        assert_eq!(repository.count(&repository.dispatch_calls), 1);
        assert_eq!(repository.count(&repository.settlement_calls), 1);
    }

    #[test]
    fn concurrent_duplicate_invocations_singleflight_one_dispatch_one_charge() {
        let repository = FakeRuntimeAuthorityRepository::protocol(None);
        let admission_service = Arc::new(service(repository.clone()));
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let service = admission_service.clone();
                let barrier = barrier.clone();
                let repository_for_task = repository.clone();
                std::thread::spawn(move || -> Result<RuntimeAdmissionDecisionV1, String> {
                    barrier.wait();
                    let decision = tokio_test::block_on(service.admit(context()))?;
                    if let RuntimeAdmissionDecisionV1::Admitted { receipt } = &decision {
                        service.mark_dispatching(&receipt.invocation_id)?;
                        repository_for_task.invoke_dispatch();
                        service.finalize(&receipt.invocation_id, successful_finalization())?;
                    }
                    Ok(decision)
                })
            })
            .collect();
        barrier.wait();
        let mut decisions = handles
            .into_iter()
            .map(|handle| handle.join().expect("task").expect("decision"));
        let first = decisions.next().expect("first");
        let second = decisions.next().expect("second");
        assert!(
            matches!(first, RuntimeAdmissionDecisionV1::Admitted { .. })
                || matches!(second, RuntimeAdmissionDecisionV1::Admitted { .. })
        );
        assert!(
            matches!(
                first,
                RuntimeAdmissionDecisionV1::InProgress { .. }
                    | RuntimeAdmissionDecisionV1::Replay { .. }
            ) || matches!(
                second,
                RuntimeAdmissionDecisionV1::InProgress { .. }
                    | RuntimeAdmissionDecisionV1::Replay { .. }
            )
        );
        assert_eq!(repository.count(&repository.max_concurrent_admit_calls), 1);
        assert_eq!(repository.count(&repository.cost_consumption_calls), 1);
        assert_eq!(repository.count(&repository.dispatch_calls), 1);
        assert_eq!(repository.count(&repository.settlement_calls), 1);
    }

    #[tokio::test]
    async fn disabled_service_rejects_mutating_admission_operations_without_store_calls() {
        let repository = FakeRuntimeAuthorityRepository::with_admissions(vec![]);
        let admission_service = RuntimeAdmissionService::new_with_enabled(
            repository.clone(),
            Arc::new(FakeClock::fixed()),
            1024,
            60,
            false,
        )
        .expect("service");
        assert_eq!(
            admission_service
                .admit(context())
                .await
                .expect_err("disabled"),
            "runtime_admission_disabled"
        );
        assert_eq!(
            admission_service
                .mark_dispatching("invocation-1")
                .expect_err("disabled"),
            "runtime_admission_disabled"
        );
        assert_eq!(
            admission_service
                .mark_outcome_unknown("invocation-1", "panic".into())
                .expect_err("disabled"),
            "runtime_admission_disabled"
        );
        assert_eq!(repository.count(&repository.approval_lookup_calls), 0);
        assert_eq!(repository.count(&repository.cost_consumption_calls), 0);
    }

    #[tokio::test]
    async fn singleflight_entries_are_removed_after_denied_invocations() {
        let repository = FakeRuntimeAuthorityRepository::with_admissions(
            (0..8)
                .map(|_| StoreAdmission::Denied {
                    reason_code: "denied".into(),
                })
                .collect(),
        );
        let admission_service = service(repository);
        for index in 0..8 {
            let mut request = context();
            request.invocation_id = format!("invocation-{index}");
            admission_service.admit(request).await.expect("decision");
        }
        assert_eq!(admission_service.singleflight_len(), 0);
    }

    #[test]
    fn receipt_bound_is_limited_to_store_contract() {
        let repository = FakeRuntimeAuthorityRepository::with_admissions(vec![]);
        let service = RuntimeAdmissionService::new(
            repository.clone(),
            Arc::new(FakeClock::fixed()),
            65_536,
            60,
        )
        .expect("service");
        assert_eq!(service.receipt_max_bytes(), 65_536);
        assert!(
            RuntimeAdmissionService::new(repository, Arc::new(FakeClock::fixed()), 65_537, 60)
                .is_err()
        );
    }

    #[test]
    fn recover_after_restart_marks_even_fresh_dispatch_outcome_unknown() {
        let dispatching = receipt(ExecutionReceiptStatus::Dispatching);
        let repository = FakeRuntimeAuthorityRepository::protocol(Some(dispatching));
        let clock = FakeClock::fixed();
        let admission_service =
            RuntimeAdmissionService::new(repository.clone(), Arc::new(clock.clone()), 1024, 60)
                .expect("service");
        assert_eq!(
            admission_service.recover_after_restart().expect("recovery"),
            1
        );
        let recovered = admission_service
            .read_receipt("invocation-1")
            .expect("receipt")
            .expect("stored");
        assert_eq!(recovered.status, ExecutionReceiptStatus::OutcomeUnknown);
        assert!(repository.held_unknown.load(Ordering::SeqCst));
        assert_eq!(repository.count(&repository.reconcile_calls), 1);
        assert_eq!(
            repository.reconcile_args.lock().expect("args").as_slice(),
            &[(now(), now())]
        );
    }

    #[tokio::test]
    async fn admit_sweeps_stale_dispatches_using_advanced_clock() {
        let repository = FakeRuntimeAuthorityRepository::protocol(Some(receipt(
            ExecutionReceiptStatus::Dispatching,
        )));
        let clock = FakeClock::fixed();
        let admission_service =
            RuntimeAdmissionService::new(repository.clone(), Arc::new(clock.clone()), 1024, 60)
                .expect("service");

        let first = admission_service.admit(context()).await.expect("first");
        assert!(matches!(
            first,
            RuntimeAdmissionDecisionV1::InProgress { .. }
        ));
        assert!(!repository.held_unknown.load(Ordering::SeqCst));
        assert_eq!(
            repository.reconcile_args.lock().expect("args")[0],
            (now() - Duration::seconds(60), now())
        );

        clock.advance(61);
        let second = admission_service.admit(context()).await.expect("second");
        assert!(matches!(
            second,
            RuntimeAdmissionDecisionV1::OutcomeUnknown { .. }
        ));
        assert!(repository.held_unknown.load(Ordering::SeqCst));
        assert_eq!(repository.count(&repository.reconcile_calls), 2);
        let advanced = now() + Duration::seconds(61);
        assert_eq!(
            repository.reconcile_args.lock().expect("args")[1],
            (advanced - Duration::seconds(60), advanced)
        );
    }
}
