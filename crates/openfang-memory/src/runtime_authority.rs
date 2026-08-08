//! Persistent, fail-closed authority admission state for runtime dispatch.

use crate::migration::run_migrations;
use chrono::{DateTime, Duration, Utc};
use openfang_types::error::{OpenFangError, OpenFangResult};
use openfang_types::runtime_admission::{
    ExecutionReceiptStatus, ExecutionReceiptV1, ReceiptFinalStatus, ReceiptFinalizationV1,
    ResultStorageMode, RuntimeAdmissionContextV1, RuntimeCostReservationOutcomeV1,
    RuntimeCostReservationRecordV1, RuntimeCostReservationRequestV1, RuntimePlanApprovalOutcomeV1,
    RuntimePlanApprovalRequestV1,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const RECEIPT_MAX_BYTES: usize = 65_536;

/// Closed persistence outcomes for an attempted runtime admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreAdmission {
    Denied { reason_code: String },
    PendingApproval { approval_ref: String },
    Admitted { receipt: ExecutionReceiptV1 },
    InProgress { receipt: ExecutionReceiptV1 },
    Replay { receipt: ExecutionReceiptV1 },
    OutcomeUnknown { receipt: ExecutionReceiptV1 },
}

/// SQLite-backed runtime authority store.
#[derive(Clone)]
pub struct RuntimeAuthorityStore {
    conn: Arc<Mutex<Connection>>,
}

impl RuntimeAuthorityStore {
    /// Initialize an authority store and bring its connection to schema v9.
    pub fn new(conn: Arc<Mutex<Connection>>) -> OpenFangResult<Self> {
        {
            let db = conn.lock().map_err(lock_error)?;
            run_migrations(&db).map_err(memory_error)?;
        }
        Ok(Self { conn })
    }

    /// Create one plan approval, retaining the closed approval lifecycle.
    pub fn request_plan_approval(
        &self,
        request: RuntimePlanApprovalRequestV1,
        auto_approve: bool,
        now: DateTime<Utc>,
    ) -> OpenFangResult<RuntimePlanApprovalOutcomeV1> {
        request.validate().map_err(invalid_input)?;
        let expires_at = expires_at(now, request.ttl_seconds)?;
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        if let Some(existing) = approval_for_plan(&tx, &request)? {
            let outcome = if existing.matches_request(&request) {
                approval_outcome(existing, now)?
            } else {
                RuntimePlanApprovalOutcomeV1::Denied {
                    reason_code: "approval_request_mismatch".into(),
                }
            };
            tx.commit().map_err(memory_error)?;
            return Ok(outcome);
        }

        let approval_ref = opaque_ref("approval");
        let status = if auto_approve { "approved" } else { "pending" };
        tx.execute(
            "INSERT INTO runtime_approvals (approval_ref, correlation_id, plan_id, plan_revision, space_id, agent_id, max_plan_cost_microusd, ttl_seconds, status, created_at, expires_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![approval_ref, request.correlation_id, request.plan_id, to_i64(request.plan_revision, "plan_revision")?, request.space_id, request.agent_id, to_i64(request.max_plan_cost_microusd, "max_plan_cost_microusd")?, to_i64(request.ttl_seconds, "ttl_seconds")?, status, timestamp(now), timestamp(expires_at), if auto_approve { Some(timestamp(now)) } else { None }],
        ).map_err(memory_error)?;
        tx.commit().map_err(memory_error)?;
        Ok(if auto_approve {
            RuntimePlanApprovalOutcomeV1::Approved {
                approval_ref,
                expires_at,
            }
        } else {
            RuntimePlanApprovalOutcomeV1::PendingApproval { approval_ref }
        })
    }

    /// Resolve a pending approval exactly once.
    pub fn resolve_plan_approval(
        &self,
        approval_ref: &str,
        approved: bool,
        now: DateTime<Utc>,
    ) -> OpenFangResult<()> {
        if approval_ref.trim().is_empty() {
            return Err(invalid_input("approval_ref must not be empty"));
        }
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        let row = approval_by_ref(&tx, approval_ref)?
            .ok_or_else(|| invalid_input("approval_ref not found"))?;
        if row.expires_at <= now {
            tx.execute("UPDATE runtime_approvals SET status = 'expired', resolved_at = ?2 WHERE approval_ref = ?1", params![approval_ref, timestamp(now)]).map_err(memory_error)?;
            tx.commit().map_err(memory_error)?;
            return Err(invalid_input("approval_ref expired"));
        }
        if row.status != "pending" {
            return Err(OpenFangError::InvalidState {
                current: row.status,
                operation: "resolve_plan_approval".into(),
            });
        }
        tx.execute(
            "UPDATE runtime_approvals SET status = ?2, resolved_at = ?3 WHERE approval_ref = ?1 AND status = 'pending'",
            params![approval_ref, if approved { "approved" } else { "denied" }, timestamp(now)],
        ).map_err(memory_error)?;
        tx.commit().map_err(memory_error)
    }

    /// Reserve one exact invocation cost without exceeding its approval ceiling.
    pub fn reserve_cost(
        &self,
        request: RuntimeCostReservationRequestV1,
        now: DateTime<Utc>,
    ) -> OpenFangResult<RuntimeCostReservationOutcomeV1> {
        request.validate().map_err(invalid_input)?;
        let expires_at = expires_at(now, request.ttl_seconds)?;
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        let existing = reservation_by_invocation(&tx, &request.invocation_id)?;
        if let Some(existing) = existing.as_ref() {
            if !existing.matches_request(&request) {
                tx.commit().map_err(memory_error)?;
                return Ok(cost_denied("cost_binding_mismatch"));
            }
            match existing.status.as_str() {
                "consumed" | "settled" | "held_unknown" => {
                    let reservation = existing.clone().into_record()?;
                    tx.commit().map_err(memory_error)?;
                    return Ok(RuntimeCostReservationOutcomeV1::Reserved { reservation });
                }
                "released" => {
                    tx.commit().map_err(memory_error)?;
                    return Ok(cost_denied("cost_released"));
                }
                "reserved" if existing.expires_at <= now => {
                    tx.execute(
                        "UPDATE cost_reservations SET status = 'released' WHERE cost_ref = ?1 AND status = 'reserved' AND expires_at <= ?2",
                        params![existing.cost_ref, timestamp(now)],
                    )
                    .map_err(memory_error)?;
                    tx.commit().map_err(memory_error)?;
                    return Ok(cost_denied("cost_expired"));
                }
                "reserved" => {}
                _ => {
                    tx.commit().map_err(memory_error)?;
                    return Ok(cost_denied("cost_unavailable"));
                }
            }
        }
        let Some(approval) = approval_by_ref(&tx, &request.approval_ref)? else {
            tx.commit().map_err(memory_error)?;
            return Ok(cost_denied("approval_missing"));
        };
        if !approval.matches_cost_request(&request) {
            tx.commit().map_err(memory_error)?;
            return Ok(cost_denied("approval_binding_mismatch"));
        }
        if approval.expires_at <= now {
            tx.execute(
                "UPDATE runtime_approvals SET status = 'expired' WHERE approval_ref = ?1",
                [&request.approval_ref],
            )
            .map_err(memory_error)?;
            tx.commit().map_err(memory_error)?;
            return Ok(cost_denied("approval_expired"));
        }
        if approval.status != "approved" {
            tx.commit().map_err(memory_error)?;
            return Ok(cost_denied("approval_unavailable"));
        }
        if let Some(existing) = existing {
            let outcome = RuntimeCostReservationOutcomeV1::Reserved {
                reservation: existing.into_record()?,
            };
            tx.commit().map_err(memory_error)?;
            return Ok(outcome);
        }
        tx.execute(
            "UPDATE cost_reservations SET status = 'released' WHERE approval_ref = ?1 AND status = 'reserved' AND expires_at <= ?2",
            params![request.approval_ref, timestamp(now)],
        )
        .map_err(memory_error)?;
        let allocated: i64 = tx.query_row(
            "SELECT COALESCE(SUM(reserved_micro_usd), 0) FROM cost_reservations WHERE approval_ref = ?1 AND status != 'released'",
            [&request.approval_ref],
            |row| row.get::<_, i64>(0),
        ).map_err(memory_error)?;
        let allocated = from_i64(allocated, "allocated_micro_usd")?;
        let requested = request.max_cost_microusd;
        if allocated
            .checked_add(requested)
            .ok_or_else(|| invalid_input("cost total overflow"))?
            > approval.max_plan_cost_microusd
        {
            tx.commit().map_err(memory_error)?;
            return Ok(cost_denied("approval_budget_exceeded"));
        }
        let cost_ref = opaque_ref("cost");
        tx.execute(
            "INSERT INTO cost_reservations (cost_ref, approval_ref, invocation_id, correlation_id, plan_id, plan_revision, space_id, agent_id, reserved_micro_usd, ttl_seconds, actual_cost_micro_usd, status, issued_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, 'reserved', ?11, ?12)",
            params![cost_ref, request.approval_ref, request.invocation_id, request.correlation_id, request.plan_id, to_i64(request.plan_revision, "plan_revision")?, request.space_id, request.agent_id, to_i64(request.max_cost_microusd, "max_cost_microusd")?, to_i64(request.ttl_seconds, "ttl_seconds")?, timestamp(now), timestamp(expires_at)],
        ).map_err(memory_error)?;
        let reservation = ReservationRow::from_request(cost_ref, &request, now, expires_at)?;
        tx.commit().map_err(memory_error)?;
        Ok(RuntimeCostReservationOutcomeV1::Reserved {
            reservation: reservation.into_record()?,
        })
    }

    /// Atomically consume a matching reservation and create exactly one prepared receipt.
    pub fn admit(
        &self,
        context: RuntimeAdmissionContextV1,
        now: DateTime<Utc>,
    ) -> OpenFangResult<StoreAdmission> {
        context.validate().map_err(invalid_input)?;
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        if let Some(receipt) = receipt_by_invocation(&tx, &context.invocation_id)? {
            let outcome = if receipt.matches_context(&context) {
                classify_receipt(receipt.into_receipt()?)
            } else {
                StoreAdmission::Denied {
                    reason_code: "receipt_binding_mismatch".into(),
                }
            };
            tx.commit().map_err(memory_error)?;
            return Ok(outcome);
        }
        let Some(approval) = approval_by_ref(&tx, &context.approval_ref)? else {
            tx.commit().map_err(memory_error)?;
            return Ok(StoreAdmission::Denied {
                reason_code: "approval_missing".into(),
            });
        };
        if approval.expires_at <= now {
            tx.execute(
                "UPDATE runtime_approvals SET status = 'expired' WHERE approval_ref = ?1",
                [&context.approval_ref],
            )
            .map_err(memory_error)?;
            tx.commit().map_err(memory_error)?;
            return Ok(StoreAdmission::Denied {
                reason_code: "approval_expired".into(),
            });
        }
        if approval.status == "pending" {
            tx.commit().map_err(memory_error)?;
            return Ok(StoreAdmission::PendingApproval {
                approval_ref: context.approval_ref,
            });
        }
        if approval.status != "approved" || approval.agent_id != context.caller_agent_id {
            tx.commit().map_err(memory_error)?;
            return Ok(StoreAdmission::Denied {
                reason_code: "approval_binding_mismatch".into(),
            });
        }
        let Some(cost) = reservation_by_ref(&tx, &context.cost_ref)? else {
            tx.commit().map_err(memory_error)?;
            return Ok(StoreAdmission::Denied {
                reason_code: "cost_missing".into(),
            });
        };
        if !cost.matches_admission(&context, &approval)
            || cost.expires_at <= now
            || cost.status != "reserved"
        {
            tx.commit().map_err(memory_error)?;
            return Ok(StoreAdmission::Denied {
                reason_code: "cost_unavailable".into(),
            });
        }
        let receipt = ReceiptRow::prepared(&context, &approval, now);
        tx.execute(
            "INSERT INTO execution_receipts (invocation_id, correlation_id, plan_id, plan_revision, space_id, agent_id, tool_name, arguments_sha256, approval_ref, cost_ref, status, result_sha256, result_storage_mode, result_envelope, error_class, prepared_at, dispatch_started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'prepared', NULL, NULL, NULL, NULL, ?11, NULL, NULL)",
            params![receipt.invocation_id, receipt.correlation_id, receipt.plan_id, to_i64(receipt.plan_revision, "plan_revision")?, receipt.space_id, receipt.agent_id, receipt.tool_name, receipt.arguments_sha256, receipt.approval_ref, receipt.cost_ref, timestamp(receipt.prepared_at)],
        ).map_err(memory_error)?;
        let consumed = tx.execute(
            "UPDATE cost_reservations SET status = 'consumed' WHERE cost_ref = ?1 AND status = 'reserved'",
            [&context.cost_ref],
        ).map_err(memory_error)?;
        if consumed != 1 {
            return Err(OpenFangError::InvalidState {
                current: "reservation changed".into(),
                operation: "admit".into(),
            });
        }
        tx.commit().map_err(memory_error)?;
        Ok(StoreAdmission::Admitted {
            receipt: receipt.into_receipt()?,
        })
    }

    /// Transition only a prepared receipt to dispatching.
    pub fn mark_dispatching(
        &self,
        invocation_id: &str,
        now: DateTime<Utc>,
    ) -> OpenFangResult<ExecutionReceiptV1> {
        if invocation_id.trim().is_empty() {
            return Err(invalid_input("invocation_id must not be empty"));
        }
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        let updated = tx.execute(
            "UPDATE execution_receipts SET status = 'dispatching', dispatch_started_at = ?2 WHERE invocation_id = ?1 AND status = 'prepared'",
            params![invocation_id, timestamp(now)],
        ).map_err(memory_error)?;
        if updated != 1 {
            return Err(OpenFangError::InvalidState {
                current: "not_prepared_or_missing".into(),
                operation: "mark_dispatching".into(),
            });
        }
        let receipt = receipt_by_invocation(&tx, invocation_id)?
            .ok_or_else(|| invalid_input("receipt missing"))?
            .into_receipt()?;
        tx.commit().map_err(memory_error)?;
        Ok(receipt)
    }

    /// Finalize a dispatch and settle its cost, retaining unknown outcomes as held.
    pub fn finalize(
        &self,
        invocation_id: &str,
        update: ReceiptFinalizationV1,
        now: DateTime<Utc>,
    ) -> OpenFangResult<ExecutionReceiptV1> {
        if invocation_id.trim().is_empty() {
            return Err(invalid_input("invocation_id must not be empty"));
        }
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        let receipt = receipt_by_invocation(&tx, invocation_id)?
            .ok_or_else(|| invalid_input("receipt missing"))?;
        if receipt.status != "dispatching" {
            return Err(OpenFangError::InvalidState {
                current: receipt.status,
                operation: "finalize".into(),
            });
        }
        let reservation = reservation_by_ref(&tx, &receipt.cost_ref)?
            .ok_or_else(|| invalid_input("reservation missing"))?;
        let record = reservation.clone().into_record()?;
        update
            .validate_against(&record, RECEIPT_MAX_BYTES)
            .map_err(invalid_input)?;
        let status = final_status(update.status);
        let storage_mode = update.result_storage_mode.map(storage_mode);
        let receipt_updated = tx.execute(
            "UPDATE execution_receipts SET status = ?2, result_sha256 = ?3, result_storage_mode = ?4, result_envelope = ?5, error_class = ?6, finished_at = ?7 WHERE invocation_id = ?1 AND status = 'dispatching'",
            params![invocation_id, status, update.result_sha256, storage_mode, update.result_envelope, update.error_class, timestamp(now)],
        ).map_err(memory_error)?;
        if receipt_updated != 1 {
            return Err(OpenFangError::InvalidState {
                current: "receipt changed".into(),
                operation: "finalize".into(),
            });
        }
        let cost_updated = tx.execute(
            "UPDATE cost_reservations SET status = ?2, actual_cost_micro_usd = ?3 WHERE cost_ref = ?1 AND status = 'consumed'",
            params![receipt.cost_ref, if matches!(update.status, ReceiptFinalStatus::OutcomeUnknown) { "held_unknown" } else { "settled" }, to_i64(update.actual_cost_micro_usd, "actual_cost_micro_usd")?],
        ).map_err(memory_error)?;
        if cost_updated != 1 {
            return Err(OpenFangError::InvalidState {
                current: "reservation changed".into(),
                operation: "finalize".into(),
            });
        }
        let finalized = receipt_by_invocation(&tx, invocation_id)?
            .ok_or_else(|| invalid_input("receipt missing"))?
            .into_receipt()?;
        tx.commit().map_err(memory_error)?;
        Ok(finalized)
    }

    /// Atomically preserve a dispatch outcome that cannot be determined after the boundary.
    ///
    /// Unlike normal finalization, this never invents an actual cost: the consumed
    /// reservation becomes `held_unknown` and its nullable cost remains unchanged.
    pub fn mark_outcome_unknown(
        &self,
        invocation_id: &str,
        error_class: &str,
        now: DateTime<Utc>,
    ) -> OpenFangResult<ExecutionReceiptV1> {
        if invocation_id.trim().is_empty() || error_class.trim().is_empty() {
            return Err(invalid_input(
                "invocation_id and error_class must not be empty",
            ));
        }
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        let receipt = receipt_by_invocation(&tx, invocation_id)?
            .ok_or_else(|| invalid_input("receipt missing"))?;
        if receipt.status != "dispatching" {
            return Err(OpenFangError::InvalidState {
                current: receipt.status,
                operation: "mark_outcome_unknown".into(),
            });
        }
        let receipt_updated = tx.execute(
            "UPDATE execution_receipts SET status = 'outcome_unknown', error_class = ?2, finished_at = ?3 WHERE invocation_id = ?1 AND status = 'dispatching'",
            params![invocation_id, error_class, timestamp(now)],
        ).map_err(memory_error)?;
        if receipt_updated != 1 {
            return Err(OpenFangError::InvalidState {
                current: "receipt changed".into(),
                operation: "mark_outcome_unknown".into(),
            });
        }
        let cost_updated = tx.execute(
            "UPDATE cost_reservations SET status = 'held_unknown' WHERE cost_ref = ?1 AND status = 'consumed'",
            [&receipt.cost_ref],
        ).map_err(memory_error)?;
        if cost_updated != 1 {
            return Err(OpenFangError::InvalidState {
                current: "reservation changed".into(),
                operation: "mark_outcome_unknown".into(),
            });
        }
        let updated = receipt_by_invocation(&tx, invocation_id)?
            .ok_or_else(|| invalid_input("receipt missing"))?
            .into_receipt()?;
        tx.commit().map_err(memory_error)?;
        Ok(updated)
    }

    /// Get a previously persisted receipt without changing its state.
    pub fn get_receipt(&self, invocation_id: &str) -> OpenFangResult<Option<ExecutionReceiptV1>> {
        let conn = self.conn.lock().map_err(lock_error)?;
        receipt_by_invocation(&conn, invocation_id)?
            .map(ReceiptRow::into_receipt)
            .transpose()
    }

    /// Mark stale dispatching operations outcome-unknown and permanently non-redispatchable.
    pub fn reconcile_stale_dispatching(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> OpenFangResult<u64> {
        if cutoff > now {
            return Err(invalid_input("cutoff must not be after now"));
        }
        let mut conn = self.conn.lock().map_err(lock_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(memory_error)?;
        let stale: Vec<(String, String)> = {
            let mut statement = tx
                .prepare(
                    "SELECT invocation_id, cost_ref FROM execution_receipts WHERE status = 'dispatching' AND dispatch_started_at <= ?1",
                )
                .map_err(memory_error)?;
            let rows = statement
                .query_map([timestamp(cutoff)], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(memory_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(memory_error)?
        };
        for (invocation_id, cost_ref) in &stale {
            let receipt_updated = tx.execute(
                "UPDATE execution_receipts SET status = 'outcome_unknown', error_class = 'stale_dispatching', finished_at = ?2 WHERE invocation_id = ?1 AND status = 'dispatching'",
                params![invocation_id, timestamp(now)],
            ).map_err(memory_error)?;
            if receipt_updated != 1 {
                return Err(OpenFangError::InvalidState {
                    current: "receipt changed".into(),
                    operation: "reconcile_stale_dispatching".into(),
                });
            }
            let cost_updated = tx.execute(
                "UPDATE cost_reservations SET status = 'held_unknown' WHERE cost_ref = ?1 AND status = 'consumed'",
                [cost_ref],
            ).map_err(memory_error)?;
            if cost_updated != 1 {
                return Err(OpenFangError::InvalidState {
                    current: "reservation changed".into(),
                    operation: "reconcile_stale_dispatching".into(),
                });
            }
        }
        tx.commit().map_err(memory_error)?;
        u64::try_from(stale.len()).map_err(|_| invalid_input("receipt count overflow"))
    }

    #[cfg(test)]
    fn receipt_count(&self) -> OpenFangResult<u64> {
        self.count("execution_receipts")
    }

    #[cfg(test)]
    fn cost_status(&self, cost_ref: &str) -> OpenFangResult<String> {
        let conn = self.conn.lock().map_err(lock_error)?;
        conn.query_row(
            "SELECT status FROM cost_reservations WHERE cost_ref = ?1",
            [cost_ref],
            |row| row.get(0),
        )
        .map_err(memory_error)
    }

    #[cfg(test)]
    fn count(&self, table: &str) -> OpenFangResult<u64> {
        let conn = self.conn.lock().map_err(lock_error)?;
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(memory_error)?;
        from_i64(count, "row_count")
    }
}

#[derive(Clone)]
struct ApprovalRow {
    approval_ref: String,
    correlation_id: String,
    plan_id: String,
    plan_revision: u64,
    space_id: String,
    agent_id: String,
    max_plan_cost_microusd: u64,
    ttl_seconds: u64,
    status: String,
    expires_at: DateTime<Utc>,
}
impl ApprovalRow {
    fn matches_request(&self, r: &RuntimePlanApprovalRequestV1) -> bool {
        self.correlation_id == r.correlation_id
            && self.plan_id == r.plan_id
            && self.plan_revision == r.plan_revision
            && self.space_id == r.space_id
            && self.agent_id == r.agent_id
            && self.max_plan_cost_microusd == r.max_plan_cost_microusd
            && self.ttl_seconds == r.ttl_seconds
    }

    fn matches_cost_request(&self, r: &RuntimeCostReservationRequestV1) -> bool {
        self.approval_ref == r.approval_ref
            && self.correlation_id == r.correlation_id
            && self.plan_id == r.plan_id
            && self.plan_revision == r.plan_revision
            && self.space_id == r.space_id
            && self.agent_id == r.agent_id
    }
}

#[derive(Clone)]
struct ReservationRow {
    cost_ref: String,
    approval_ref: String,
    invocation_id: String,
    correlation_id: String,
    plan_id: String,
    plan_revision: u64,
    space_id: String,
    agent_id: String,
    reserved_micro_usd: u64,
    ttl_seconds: u64,
    status: String,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}
impl ReservationRow {
    fn from_request(
        cost_ref: String,
        r: &RuntimeCostReservationRequestV1,
        issued_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> OpenFangResult<Self> {
        Ok(Self {
            cost_ref,
            approval_ref: r.approval_ref.clone(),
            invocation_id: r.invocation_id.clone(),
            correlation_id: r.correlation_id.clone(),
            plan_id: r.plan_id.clone(),
            plan_revision: r.plan_revision,
            space_id: r.space_id.clone(),
            agent_id: r.agent_id.clone(),
            reserved_micro_usd: r.max_cost_microusd,
            ttl_seconds: r.ttl_seconds,
            status: "reserved".into(),
            issued_at,
            expires_at,
        })
    }
    fn matches_request(&self, r: &RuntimeCostReservationRequestV1) -> bool {
        self.approval_ref == r.approval_ref
            && self.invocation_id == r.invocation_id
            && self.correlation_id == r.correlation_id
            && self.plan_id == r.plan_id
            && self.plan_revision == r.plan_revision
            && self.space_id == r.space_id
            && self.agent_id == r.agent_id
            && self.reserved_micro_usd == r.max_cost_microusd
            && self.ttl_seconds == r.ttl_seconds
            && expires_at(self.issued_at, r.ttl_seconds)
                .map(|expected| self.expires_at == expected)
                .unwrap_or(false)
    }
    fn matches_admission(&self, c: &RuntimeAdmissionContextV1, a: &ApprovalRow) -> bool {
        self.cost_ref == c.cost_ref
            && self.approval_ref == c.approval_ref
            && self.invocation_id == c.invocation_id
            && self.agent_id == c.caller_agent_id
            && self.correlation_id == a.correlation_id
            && self.plan_id == a.plan_id
            && self.plan_revision == a.plan_revision
            && self.space_id == a.space_id
            && self.agent_id == a.agent_id
    }
    fn into_record(self) -> OpenFangResult<RuntimeCostReservationRecordV1> {
        let record = RuntimeCostReservationRecordV1 {
            cost_ref: self.cost_ref,
            approval_ref: self.approval_ref,
            invocation_id: self.invocation_id,
            correlation_id: self.correlation_id,
            plan_id: self.plan_id,
            plan_revision: self.plan_revision,
            space_id: self.space_id,
            agent_id: self.agent_id,
            reserved_micro_usd: self.reserved_micro_usd,
            issued_at: self.issued_at,
            expires_at: self.expires_at,
        };
        record.validate().map_err(invalid_input)?;
        Ok(record)
    }
}

#[derive(Clone)]
struct ReceiptRow {
    invocation_id: String,
    correlation_id: String,
    plan_id: String,
    plan_revision: u64,
    space_id: String,
    agent_id: String,
    tool_name: String,
    arguments_sha256: String,
    approval_ref: String,
    cost_ref: String,
    status: String,
    result_sha256: Option<String>,
    result_storage_mode: Option<String>,
    result_envelope: Option<String>,
    error_class: Option<String>,
    prepared_at: DateTime<Utc>,
    dispatch_started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}
impl ReceiptRow {
    fn prepared(c: &RuntimeAdmissionContextV1, a: &ApprovalRow, now: DateTime<Utc>) -> Self {
        Self {
            invocation_id: c.invocation_id.clone(),
            correlation_id: a.correlation_id.clone(),
            plan_id: a.plan_id.clone(),
            plan_revision: a.plan_revision,
            space_id: a.space_id.clone(),
            agent_id: a.agent_id.clone(),
            tool_name: c.tool_name.clone(),
            arguments_sha256: c.arguments_sha256.clone(),
            approval_ref: c.approval_ref.clone(),
            cost_ref: c.cost_ref.clone(),
            status: "prepared".into(),
            result_sha256: None,
            result_storage_mode: None,
            result_envelope: None,
            error_class: None,
            prepared_at: now,
            dispatch_started_at: None,
            finished_at: None,
        }
    }
    fn matches_context(&self, c: &RuntimeAdmissionContextV1) -> bool {
        self.invocation_id == c.invocation_id
            && self.agent_id == c.caller_agent_id
            && self.approval_ref == c.approval_ref
            && self.cost_ref == c.cost_ref
            && self.tool_name == c.tool_name
            && self.arguments_sha256 == c.arguments_sha256
    }
    fn into_receipt(self) -> OpenFangResult<ExecutionReceiptV1> {
        let receipt = ExecutionReceiptV1 {
            invocation_id: self.invocation_id,
            correlation_id: self.correlation_id,
            plan_id: self.plan_id,
            plan_revision: self.plan_revision,
            space_id: self.space_id,
            agent_id: self.agent_id,
            tool_name: self.tool_name,
            arguments_sha256: self.arguments_sha256,
            approval_ref: self.approval_ref,
            cost_ref: self.cost_ref,
            status: receipt_status(&self.status)?,
            result_sha256: self.result_sha256,
            result_storage_mode: self
                .result_storage_mode
                .map(parse_storage_mode)
                .transpose()?,
            result_envelope: self.result_envelope,
            error_class: self.error_class,
            prepared_at: self.prepared_at,
            dispatch_started_at: self.dispatch_started_at,
            finished_at: self.finished_at,
        };
        receipt.validate(RECEIPT_MAX_BYTES).map_err(invalid_input)?;
        Ok(receipt)
    }
}

fn approval_for_plan(
    tx: &Transaction<'_>,
    r: &RuntimePlanApprovalRequestV1,
) -> OpenFangResult<Option<ApprovalRow>> {
    tx.query_row("SELECT approval_ref, correlation_id, plan_id, plan_revision, space_id, agent_id, max_plan_cost_microusd, ttl_seconds, status, expires_at FROM runtime_approvals WHERE plan_id = ?1 AND plan_revision = ?2 AND space_id = ?3 AND agent_id = ?4", params![r.plan_id, to_i64(r.plan_revision, "plan_revision")?, r.space_id, r.agent_id], approval_row).optional().map_err(memory_error)
}
fn approval_by_ref(conn: &Connection, approval_ref: &str) -> OpenFangResult<Option<ApprovalRow>> {
    conn.query_row("SELECT approval_ref, correlation_id, plan_id, plan_revision, space_id, agent_id, max_plan_cost_microusd, ttl_seconds, status, expires_at FROM runtime_approvals WHERE approval_ref = ?1", [approval_ref], approval_row).optional().map_err(memory_error)
}
fn reservation_by_invocation(
    conn: &Connection,
    invocation_id: &str,
) -> OpenFangResult<Option<ReservationRow>> {
    conn.query_row("SELECT cost_ref, approval_ref, invocation_id, correlation_id, plan_id, plan_revision, space_id, agent_id, reserved_micro_usd, ttl_seconds, status, issued_at, expires_at FROM cost_reservations WHERE invocation_id = ?1", [invocation_id], reservation_row).optional().map_err(memory_error)
}
fn reservation_by_ref(conn: &Connection, cost_ref: &str) -> OpenFangResult<Option<ReservationRow>> {
    conn.query_row("SELECT cost_ref, approval_ref, invocation_id, correlation_id, plan_id, plan_revision, space_id, agent_id, reserved_micro_usd, ttl_seconds, status, issued_at, expires_at FROM cost_reservations WHERE cost_ref = ?1", [cost_ref], reservation_row).optional().map_err(memory_error)
}
fn receipt_by_invocation(
    conn: &Connection,
    invocation_id: &str,
) -> OpenFangResult<Option<ReceiptRow>> {
    conn.query_row("SELECT invocation_id, correlation_id, plan_id, plan_revision, space_id, agent_id, tool_name, arguments_sha256, approval_ref, cost_ref, status, result_sha256, result_storage_mode, result_envelope, error_class, prepared_at, dispatch_started_at, finished_at FROM execution_receipts WHERE invocation_id = ?1", [invocation_id], receipt_row).optional().map_err(memory_error)
}

fn approval_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRow> {
    Ok(ApprovalRow {
        approval_ref: row.get(0)?,
        correlation_id: row.get(1)?,
        plan_id: row.get(2)?,
        plan_revision: row.get::<_, i64>(3).and_then(from_db_u64)?,
        space_id: row.get(4)?,
        agent_id: row.get(5)?,
        max_plan_cost_microusd: row.get::<_, i64>(6).and_then(from_db_u64)?,
        ttl_seconds: row.get::<_, i64>(7).and_then(from_db_u64)?,
        status: row.get(8)?,
        expires_at: row.get::<_, String>(9).and_then(parse_db_time)?,
    })
}
fn reservation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReservationRow> {
    Ok(ReservationRow {
        cost_ref: row.get(0)?,
        approval_ref: row.get(1)?,
        invocation_id: row.get(2)?,
        correlation_id: row.get(3)?,
        plan_id: row.get(4)?,
        plan_revision: row.get::<_, i64>(5).and_then(from_db_u64)?,
        space_id: row.get(6)?,
        agent_id: row.get(7)?,
        reserved_micro_usd: row.get::<_, i64>(8).and_then(from_db_u64)?,
        ttl_seconds: row.get::<_, i64>(9).and_then(from_db_u64)?,
        status: row.get(10)?,
        issued_at: row.get::<_, String>(11).and_then(parse_db_time)?,
        expires_at: row.get::<_, String>(12).and_then(parse_db_time)?,
    })
}
fn receipt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReceiptRow> {
    Ok(ReceiptRow {
        invocation_id: row.get(0)?,
        correlation_id: row.get(1)?,
        plan_id: row.get(2)?,
        plan_revision: row.get::<_, i64>(3).and_then(from_db_u64)?,
        space_id: row.get(4)?,
        agent_id: row.get(5)?,
        tool_name: row.get(6)?,
        arguments_sha256: row.get(7)?,
        approval_ref: row.get(8)?,
        cost_ref: row.get(9)?,
        status: row.get(10)?,
        result_sha256: row.get(11)?,
        result_storage_mode: row.get(12)?,
        result_envelope: row.get(13)?,
        error_class: row.get(14)?,
        prepared_at: row.get::<_, String>(15).and_then(parse_db_time)?,
        dispatch_started_at: row
            .get::<_, Option<String>>(16)?
            .map(|value| parse_db_time(value))
            .transpose()?,
        finished_at: row
            .get::<_, Option<String>>(17)?
            .map(|value| parse_db_time(value))
            .transpose()?,
    })
}

fn approval_outcome(
    row: ApprovalRow,
    now: DateTime<Utc>,
) -> OpenFangResult<RuntimePlanApprovalOutcomeV1> {
    Ok(if row.expires_at <= now || row.status == "expired" {
        RuntimePlanApprovalOutcomeV1::Denied {
            reason_code: "approval_expired".into(),
        }
    } else if row.status == "approved" {
        RuntimePlanApprovalOutcomeV1::Approved {
            approval_ref: row.approval_ref,
            expires_at: row.expires_at,
        }
    } else if row.status == "pending" {
        RuntimePlanApprovalOutcomeV1::PendingApproval {
            approval_ref: row.approval_ref,
        }
    } else {
        RuntimePlanApprovalOutcomeV1::Denied {
            reason_code: "approval_denied".into(),
        }
    })
}
fn classify_receipt(receipt: ExecutionReceiptV1) -> StoreAdmission {
    match receipt.status {
        ExecutionReceiptStatus::Prepared | ExecutionReceiptStatus::Dispatching => {
            StoreAdmission::InProgress { receipt }
        }
        ExecutionReceiptStatus::Succeeded | ExecutionReceiptStatus::Failed => {
            StoreAdmission::Replay { receipt }
        }
        ExecutionReceiptStatus::OutcomeUnknown => StoreAdmission::OutcomeUnknown { receipt },
    }
}
fn cost_denied(code: &str) -> RuntimeCostReservationOutcomeV1 {
    RuntimeCostReservationOutcomeV1::Denied {
        reason_code: code.into(),
    }
}
fn final_status(status: ReceiptFinalStatus) -> &'static str {
    match status {
        ReceiptFinalStatus::Succeeded => "succeeded",
        ReceiptFinalStatus::Failed => "failed",
        ReceiptFinalStatus::OutcomeUnknown => "outcome_unknown",
    }
}
fn receipt_status(status: &str) -> OpenFangResult<ExecutionReceiptStatus> {
    match status {
        "prepared" => Ok(ExecutionReceiptStatus::Prepared),
        "dispatching" => Ok(ExecutionReceiptStatus::Dispatching),
        "succeeded" => Ok(ExecutionReceiptStatus::Succeeded),
        "failed" => Ok(ExecutionReceiptStatus::Failed),
        "outcome_unknown" => Ok(ExecutionReceiptStatus::OutcomeUnknown),
        _ => Err(invalid_input("invalid persisted receipt status")),
    }
}
fn storage_mode(mode: ResultStorageMode) -> &'static str {
    match mode {
        ResultStorageMode::RedactedEnvelope => "redacted_envelope",
        ResultStorageMode::DigestOnly => "digest_only",
    }
}
fn parse_storage_mode(mode: String) -> OpenFangResult<ResultStorageMode> {
    match mode.as_str() {
        "redacted_envelope" => Ok(ResultStorageMode::RedactedEnvelope),
        "digest_only" => Ok(ResultStorageMode::DigestOnly),
        _ => Err(invalid_input("invalid persisted result storage mode")),
    }
}
fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}
fn expires_at(now: DateTime<Utc>, ttl_seconds: u64) -> OpenFangResult<DateTime<Utc>> {
    let seconds = i64::try_from(ttl_seconds).map_err(|_| invalid_input("ttl_seconds overflow"))?;
    let duration =
        Duration::try_seconds(seconds).ok_or_else(|| invalid_input("ttl_seconds overflow"))?;
    now.checked_add_signed(duration)
        .ok_or_else(|| invalid_input("expires_at overflow"))
}
fn opaque_ref(kind: &str) -> String {
    format!("{kind}:{}", Uuid::new_v4())
}
fn to_i64(value: u64, name: &str) -> OpenFangResult<i64> {
    i64::try_from(value).map_err(|_| invalid_input(format!("{name} exceeds SQLite integer range")))
}
fn from_i64(value: i64, name: &str) -> OpenFangResult<u64> {
    u64::try_from(value).map_err(|_| invalid_input(format!("{name} is negative")))
}
fn from_db_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}
fn parse_db_time(value: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}
fn invalid_input(message: impl Into<String>) -> OpenFangError {
    OpenFangError::InvalidInput(message.into())
}
fn memory_error(error: rusqlite::Error) -> OpenFangError {
    OpenFangError::Memory(error.to_string())
}
fn lock_error<T>(error: std::sync::PoisonError<T>) -> OpenFangError {
    OpenFangError::Memory(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use openfang_types::runtime_admission::{
        ReceiptFinalStatus, ReceiptFinalizationV1, ResultStorageMode, RuntimeAdmissionDecisionV1,
        RuntimeAuthorityContractVersion,
    };
    use rusqlite::Connection;
    use std::sync::{Arc, Barrier, Mutex};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 8, 12, 0, 0).single().unwrap()
    }
    fn test_store() -> RuntimeAuthorityStore {
        RuntimeAuthorityStore::new(Arc::new(Mutex::new(Connection::open_in_memory().unwrap())))
            .unwrap()
    }
    fn approval_request(max_cost: u64) -> RuntimePlanApprovalRequestV1 {
        RuntimePlanApprovalRequestV1 {
            contract_version: RuntimeAuthorityContractVersion::V1,
            correlation_id: "corr-1".into(),
            plan_id: "plan-1".into(),
            plan_revision: 1,
            space_id: "ideas".into(),
            agent_id: "agent-1".into(),
            max_plan_cost_microusd: max_cost,
            ttl_seconds: 300,
        }
    }
    fn reserve_request(
        approval_ref: String,
        invocation_id: &str,
        cost: u64,
    ) -> RuntimeCostReservationRequestV1 {
        RuntimeCostReservationRequestV1 {
            contract_version: RuntimeAuthorityContractVersion::V1,
            correlation_id: "corr-1".into(),
            plan_id: "plan-1".into(),
            plan_revision: 1,
            space_id: "ideas".into(),
            agent_id: "agent-1".into(),
            approval_ref,
            invocation_id: invocation_id.into(),
            max_cost_microusd: cost,
            ttl_seconds: 300,
        }
    }
    fn approval_and_reservation(store: &RuntimeAuthorityStore, cost: u64) -> (String, String) {
        let approval_ref = match store
            .request_plan_approval(approval_request(cost), true, now())
            .unwrap()
        {
            RuntimePlanApprovalOutcomeV1::Approved { approval_ref, .. } => approval_ref,
            other => panic!("expected approved outcome, got {other:?}"),
        };
        let cost_ref = match store
            .reserve_cost(
                reserve_request(approval_ref.clone(), "invocation-1", cost),
                now(),
            )
            .unwrap()
        {
            RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation.cost_ref,
            other => panic!("expected reservation, got {other:?}"),
        };
        (approval_ref, cost_ref)
    }

    #[test]
    fn shared_connection_construction_and_clone_observe_the_same_state() {
        let connection = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let store = RuntimeAuthorityStore::new(Arc::clone(&connection)).unwrap();
        let cloned = store.clone();

        let first = store
            .request_plan_approval(approval_request(3), true, now())
            .unwrap();
        let observed = cloned
            .request_plan_approval(approval_request(3), true, now())
            .unwrap();
        assert_eq!(observed, first);

        let approval_count: i64 = connection
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM runtime_approvals", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(approval_count, 1);
    }
    fn context(approval_ref: String, cost_ref: String) -> RuntimeAdmissionContextV1 {
        RuntimeAdmissionContextV1 {
            invocation_id: "invocation-1".into(),
            caller_agent_id: "agent-1".into(),
            approval_ref,
            cost_ref,
            tool_name: "notes.search".into(),
            arguments_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn concurrent_duplicate_admission_consumes_cost_once_and_creates_one_receipt() {
        let store = Arc::new(test_store());
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 7);
        let gate = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let store = Arc::clone(&store);
                let gate = Arc::clone(&gate);
                let context = context(approval_ref.clone(), cost_ref.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    store.admit(context, now()).unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, StoreAdmission::Admitted { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    StoreAdmission::InProgress { .. } | StoreAdmission::Replay { .. }
                ))
                .count(),
            1
        );
        assert_eq!(store.receipt_count().unwrap(), 1);
        assert_eq!(store.cost_status(&cost_ref).unwrap(), "consumed");
    }
    #[test]
    fn same_refs_cannot_cross_agent_tool_or_arguments_digest() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 0);
        assert!(matches!(
            store
                .admit(context(approval_ref.clone(), cost_ref.clone()), now())
                .unwrap(),
            StoreAdmission::Admitted { .. }
        ));
        for mutate in [0, 1, 2] {
            let mut retry = context(approval_ref.clone(), cost_ref.clone());
            match mutate {
                0 => retry.caller_agent_id = "agent-2".into(),
                1 => retry.tool_name = "notes.write".into(),
                _ => retry.arguments_sha256 = "b".repeat(64),
            }
            assert!(matches!(
                store.admit(retry, now()).unwrap(),
                StoreAdmission::Denied { .. }
            ));
        }
    }
    #[test]
    fn approval_lifecycle_and_zero_cost_budget_accounting_are_persistent() {
        let store = test_store();
        let pending = store
            .request_plan_approval(approval_request(0), false, now())
            .unwrap();
        let approval_ref = match pending {
            RuntimePlanApprovalOutcomeV1::PendingApproval { approval_ref } => approval_ref,
            other => panic!("unexpected {other:?}"),
        };
        store
            .resolve_plan_approval(&approval_ref, true, now())
            .unwrap();
        let reserved = store
            .reserve_cost(reserve_request(approval_ref, "zero-cost", 0), now())
            .unwrap();
        assert!(matches!(
            reserved,
            RuntimeCostReservationOutcomeV1::Reserved { .. }
        ));
    }
    #[test]
    fn settlement_finalization_and_stale_recovery_never_redispatch() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
        let receipt = match store
            .admit(context(approval_ref, cost_ref.clone()), now())
            .unwrap()
        {
            StoreAdmission::Admitted { receipt } => receipt,
            other => panic!("unexpected {other:?}"),
        };
        let dispatching = store
            .mark_dispatching(&receipt.invocation_id, now())
            .unwrap();
        let unknown = store
            .reconcile_stale_dispatching(now() + Duration::seconds(1), now() + Duration::seconds(2))
            .unwrap();
        assert_eq!(unknown, 1);
        assert!(matches!(
            store
                .admit(
                    context(dispatching.approval_ref, dispatching.cost_ref),
                    now()
                )
                .unwrap(),
            StoreAdmission::OutcomeUnknown { .. }
        ));
    }
    #[test]
    fn outcome_unknown_holds_cost_without_inventing_actual_cost() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
        let receipt = match store
            .admit(context(approval_ref, cost_ref.clone()), now())
            .unwrap()
        {
            StoreAdmission::Admitted { receipt } => receipt,
            other => panic!("unexpected {other:?}"),
        };
        store
            .mark_dispatching(&receipt.invocation_id, now())
            .unwrap();
        let unknown = store
            .mark_outcome_unknown(&receipt.invocation_id, "panic", now())
            .unwrap();
        assert_eq!(unknown.status, ExecutionReceiptStatus::OutcomeUnknown);
        assert_eq!(unknown.error_class.as_deref(), Some("panic"));
        assert_eq!(store.cost_status(&cost_ref).unwrap(), "held_unknown");
        let conn = store.conn.lock().unwrap();
        let actual: Option<i64> = conn
            .query_row(
                "SELECT actual_cost_micro_usd FROM cost_reservations WHERE cost_ref = ?1",
                [&cost_ref],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actual, None);
    }
    #[test]
    fn finalization_rejects_invalid_reservation_binding_and_persists_settlement() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
        let receipt = match store
            .admit(context(approval_ref, cost_ref.clone()), now())
            .unwrap()
        {
            StoreAdmission::Admitted { receipt } => receipt,
            other => panic!("unexpected {other:?}"),
        };
        store
            .mark_dispatching(&receipt.invocation_id, now())
            .unwrap();
        let invalid = ReceiptFinalizationV1 {
            status: ReceiptFinalStatus::Succeeded,
            actual_cost_micro_usd: 4,
            result_sha256: Some("b".repeat(64)),
            result_storage_mode: Some(ResultStorageMode::DigestOnly),
            result_envelope: None,
            error_class: None,
        };
        assert!(store
            .finalize(&receipt.invocation_id, invalid, now())
            .is_err());
        let valid = ReceiptFinalizationV1 {
            status: ReceiptFinalStatus::Succeeded,
            actual_cost_micro_usd: 3,
            result_sha256: Some("b".repeat(64)),
            result_storage_mode: Some(ResultStorageMode::DigestOnly),
            result_envelope: None,
            error_class: None,
        };
        assert!(store.finalize(&receipt.invocation_id, valid, now()).is_ok());
        assert_eq!(store.cost_status(&cost_ref).unwrap(), "settled");
    }

    fn set_cost_status(store: &RuntimeAuthorityStore, cost_ref: &str, status: &str) {
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "UPDATE cost_reservations SET status = ?2 WHERE cost_ref = ?1",
            params![cost_ref, status],
        )
        .unwrap();
    }

    #[test]
    fn approval_retry_is_request_bound_and_drift_is_denied() {
        let store = test_store();
        let original = approval_request(7);
        let first = store
            .request_plan_approval(original.clone(), true, now())
            .unwrap();
        let retry = store
            .request_plan_approval(original.clone(), true, now() + Duration::seconds(1))
            .unwrap();
        assert_eq!(retry, first, "exact retries retain the original expiry");

        for mutation in [0, 1, 2] {
            let mut drift = original.clone();
            match mutation {
                0 => drift.correlation_id = "corr-2".into(),
                1 => drift.max_plan_cost_microusd = 8,
                _ => drift.ttl_seconds = 301,
            }
            assert!(matches!(
                store.request_plan_approval(drift, true, now()).unwrap(),
                RuntimePlanApprovalOutcomeV1::Denied { reason_code }
                    if reason_code == "approval_request_mismatch"
            ));
        }
    }

    #[test]
    fn denied_pending_and_expired_approvals_cannot_reserve_or_admit() {
        let store = test_store();
        let pending_ref = match store
            .request_plan_approval(approval_request(1), false, now())
            .unwrap()
        {
            RuntimePlanApprovalOutcomeV1::PendingApproval { approval_ref } => approval_ref,
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(
            store
                .reserve_cost(reserve_request(pending_ref.clone(), "pending", 1), now())
                .unwrap(),
            RuntimeCostReservationOutcomeV1::Denied { .. }
        ));
        assert!(!matches!(
            store
                .admit(context(pending_ref, "missing-cost".into()), now())
                .unwrap(),
            StoreAdmission::Admitted { .. }
        ));

        let mut denied_request = approval_request(2);
        denied_request.plan_id = "denied-plan".into();
        let denied_ref = match store
            .request_plan_approval(denied_request, false, now())
            .unwrap()
        {
            RuntimePlanApprovalOutcomeV1::PendingApproval { approval_ref } => approval_ref,
            other => panic!("unexpected {other:?}"),
        };
        store
            .resolve_plan_approval(&denied_ref, false, now())
            .unwrap();
        let mut denied_cost = reserve_request(denied_ref.clone(), "denied", 1);
        denied_cost.plan_id = "denied-plan".into();
        assert!(matches!(
            store.reserve_cost(denied_cost, now()).unwrap(),
            RuntimeCostReservationOutcomeV1::Denied { .. }
        ));
        assert!(matches!(
            store
                .admit(context(denied_ref, "missing-cost".into()), now())
                .unwrap(),
            StoreAdmission::Denied { .. }
        ));

        let mut expired = approval_request(3);
        expired.plan_id = "expired-plan".into();
        expired.ttl_seconds = 1;
        let expired_ref = match store.request_plan_approval(expired, true, now()).unwrap() {
            RuntimePlanApprovalOutcomeV1::Approved { approval_ref, .. } => approval_ref,
            other => panic!("unexpected {other:?}"),
        };
        let mut expired_cost = reserve_request(expired_ref.clone(), "expired", 1);
        expired_cost.plan_id = "expired-plan".into();
        assert!(matches!(
            store
                .reserve_cost(expired_cost, now() + Duration::seconds(2))
                .unwrap(),
            RuntimeCostReservationOutcomeV1::Denied { .. }
        ));
        assert!(matches!(
            store
                .admit(
                    context(expired_ref, "missing-cost".into()),
                    now() + Duration::seconds(2)
                )
                .unwrap(),
            StoreAdmission::Denied { .. }
        ));
    }

    #[test]
    fn expired_reserved_cost_is_released_and_duplicate_is_denied() {
        let store = test_store();
        let approval_ref = match store
            .request_plan_approval(approval_request(5), true, now())
            .unwrap()
        {
            RuntimePlanApprovalOutcomeV1::Approved { approval_ref, .. } => approval_ref,
            other => panic!("unexpected {other:?}"),
        };
        let mut first_request = reserve_request(approval_ref.clone(), "expired-invocation", 5);
        first_request.ttl_seconds = 1;
        let first_cost_ref = match store.reserve_cost(first_request.clone(), now()).unwrap() {
            RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation.cost_ref,
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(
            store
                .reserve_cost(first_request, now() + Duration::seconds(2))
                .unwrap(),
            RuntimeCostReservationOutcomeV1::Denied { reason_code }
                if reason_code == "cost_expired"
        ));
        assert_eq!(store.cost_status(&first_cost_ref).unwrap(), "released");

        let mut replacement = reserve_request(approval_ref, "replacement-invocation", 5);
        replacement.ttl_seconds = 1;
        assert!(matches!(
            store
                .reserve_cost(replacement, now() + Duration::seconds(2))
                .unwrap(),
            RuntimeCostReservationOutcomeV1::Reserved { .. }
        ));
    }

    #[test]
    fn finalize_cost_transition_failure_leaves_receipt_dispatching() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
        let receipt = match store
            .admit(context(approval_ref, cost_ref.clone()), now())
            .unwrap()
        {
            StoreAdmission::Admitted { receipt } => receipt,
            other => panic!("unexpected {other:?}"),
        };
        store
            .mark_dispatching(&receipt.invocation_id, now())
            .unwrap();
        set_cost_status(&store, &cost_ref, "held_unknown");
        let valid = ReceiptFinalizationV1 {
            status: ReceiptFinalStatus::Succeeded,
            actual_cost_micro_usd: 3,
            result_sha256: Some("b".repeat(64)),
            result_storage_mode: Some(ResultStorageMode::DigestOnly),
            result_envelope: None,
            error_class: None,
        };
        assert!(store
            .finalize(&receipt.invocation_id, valid, now())
            .is_err());
        assert_eq!(
            store
                .get_receipt(&receipt.invocation_id)
                .unwrap()
                .unwrap()
                .status,
            ExecutionReceiptStatus::Dispatching
        );
    }

    #[test]
    fn stale_recovery_invalid_cost_state_rolls_back_receipt() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
        let receipt = match store
            .admit(context(approval_ref, cost_ref.clone()), now())
            .unwrap()
        {
            StoreAdmission::Admitted { receipt } => receipt,
            other => panic!("unexpected {other:?}"),
        };
        store
            .mark_dispatching(&receipt.invocation_id, now())
            .unwrap();
        set_cost_status(&store, &cost_ref, "settled");
        assert!(store
            .reconcile_stale_dispatching(now() + Duration::seconds(1), now() + Duration::seconds(2))
            .is_err());
        assert_eq!(
            store
                .get_receipt(&receipt.invocation_id)
                .unwrap()
                .unwrap()
                .status,
            ExecutionReceiptStatus::Dispatching
        );
    }

    #[test]
    fn later_exact_cost_retry_returns_original_reservation_and_ttl_drift_is_denied() {
        let store = test_store();
        let approval_ref = match store
            .request_plan_approval(approval_request(5), true, now())
            .unwrap()
        {
            RuntimePlanApprovalOutcomeV1::Approved { approval_ref, .. } => approval_ref,
            other => panic!("unexpected {other:?}"),
        };
        let request = reserve_request(approval_ref, "later-retry", 5);
        let first = match store.reserve_cost(request.clone(), now()).unwrap() {
            RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation,
            other => panic!("unexpected {other:?}"),
        };
        let retry = match store
            .reserve_cost(request.clone(), now() + Duration::seconds(1))
            .unwrap()
        {
            RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(retry, first, "exact retry must retain original reservation");

        let mut ttl_drift = request;
        ttl_drift.ttl_seconds = 301;
        assert!(matches!(
            store
                .reserve_cost(ttl_drift, now() + Duration::seconds(1))
                .unwrap(),
            RuntimeCostReservationOutcomeV1::Denied { reason_code }
                if reason_code == "cost_binding_mismatch"
        ));
    }

    #[test]
    fn future_recovery_cutoff_is_rejected_without_mutating_receipt_or_cost() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
        let receipt = match store
            .admit(context(approval_ref, cost_ref.clone()), now())
            .unwrap()
        {
            StoreAdmission::Admitted { receipt } => receipt,
            other => panic!("unexpected {other:?}"),
        };
        store
            .mark_dispatching(&receipt.invocation_id, now())
            .unwrap();
        assert!(store
            .reconcile_stale_dispatching(now() + Duration::seconds(2), now())
            .is_err());
        assert_eq!(
            store
                .get_receipt(&receipt.invocation_id)
                .unwrap()
                .unwrap()
                .status,
            ExecutionReceiptStatus::Dispatching
        );
        assert_eq!(store.cost_status(&cost_ref).unwrap(), "consumed");
    }

    #[test]
    fn retry_after_admission_reuses_consumed_cost_and_validates_in_progress() {
        let store = test_store();
        let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
        let receipt = match store
            .admit(context(approval_ref.clone(), cost_ref.clone()), now())
            .unwrap()
        {
            StoreAdmission::Admitted { receipt } => receipt,
            other => panic!("unexpected {other:?}"),
        };
        let retry = match store
            .reserve_cost(
                reserve_request(approval_ref, "invocation-1", 3),
                now() + Duration::seconds(1),
            )
            .unwrap()
        {
            RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(retry.cost_ref, cost_ref);
        let outcome = store
            .admit(context(receipt.approval_ref, retry.cost_ref), now())
            .unwrap();
        let StoreAdmission::InProgress { receipt } = outcome else {
            panic!("retry must remain in progress");
        };
        assert!(RuntimeAdmissionDecisionV1::InProgress { receipt }
            .validate(RECEIPT_MAX_BYTES)
            .is_ok());
    }

    #[test]
    fn retry_after_finalization_reuses_cost_without_redispatching() {
        for final_status in [
            ReceiptFinalStatus::Succeeded,
            ReceiptFinalStatus::Failed,
            ReceiptFinalStatus::OutcomeUnknown,
        ] {
            let store = test_store();
            let (approval_ref, cost_ref) = approval_and_reservation(&store, 3);
            let receipt = match store
                .admit(context(approval_ref.clone(), cost_ref.clone()), now())
                .unwrap()
            {
                StoreAdmission::Admitted { receipt } => receipt,
                other => panic!("unexpected {other:?}"),
            };
            store
                .mark_dispatching(&receipt.invocation_id, now())
                .unwrap();
            let finalization = match final_status {
                ReceiptFinalStatus::Succeeded => ReceiptFinalizationV1 {
                    status: final_status,
                    actual_cost_micro_usd: 3,
                    result_sha256: Some("b".repeat(64)),
                    result_storage_mode: Some(ResultStorageMode::DigestOnly),
                    result_envelope: None,
                    error_class: None,
                },
                ReceiptFinalStatus::Failed | ReceiptFinalStatus::OutcomeUnknown => {
                    ReceiptFinalizationV1 {
                        status: final_status,
                        actual_cost_micro_usd: 3,
                        result_sha256: None,
                        result_storage_mode: None,
                        result_envelope: None,
                        error_class: Some("terminal".into()),
                    }
                }
            };
            store
                .finalize(&receipt.invocation_id, finalization, now())
                .unwrap();
            let retry = match store
                .reserve_cost(
                    reserve_request(approval_ref, "invocation-1", 3),
                    now() + Duration::seconds(1),
                )
                .unwrap()
            {
                RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation,
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(retry.cost_ref, cost_ref);
            let outcome = store
                .admit(context(receipt.approval_ref, retry.cost_ref), now())
                .unwrap();
            match outcome {
                StoreAdmission::Replay { receipt } => {
                    assert!(RuntimeAdmissionDecisionV1::Replay { receipt }
                        .validate(RECEIPT_MAX_BYTES)
                        .is_ok());
                }
                StoreAdmission::OutcomeUnknown { receipt } => {
                    assert!(RuntimeAdmissionDecisionV1::OutcomeUnknown { receipt }
                        .validate(RECEIPT_MAX_BYTES)
                        .is_ok());
                }
                other => panic!("retry must not redispatch: {other:?}"),
            }
        }
    }

    #[test]
    fn exact_nonreserved_retry_survives_authority_and_reservation_expiry() {
        for final_status in [
            None,
            Some(ReceiptFinalStatus::Succeeded),
            Some(ReceiptFinalStatus::OutcomeUnknown),
        ] {
            let store = test_store();
            let mut approval = approval_request(3);
            approval.ttl_seconds = 1;
            let approval_ref = match store.request_plan_approval(approval, true, now()).unwrap() {
                RuntimePlanApprovalOutcomeV1::Approved { approval_ref, .. } => approval_ref,
                other => panic!("unexpected {other:?}"),
            };
            let mut request = reserve_request(approval_ref.clone(), "invocation-1", 3);
            request.ttl_seconds = 1;
            let original = match store.reserve_cost(request.clone(), now()).unwrap() {
                RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation,
                other => panic!("unexpected {other:?}"),
            };
            let receipt = match store
                .admit(
                    context(approval_ref.clone(), original.cost_ref.clone()),
                    now(),
                )
                .unwrap()
            {
                StoreAdmission::Admitted { receipt } => receipt,
                other => panic!("unexpected {other:?}"),
            };
            if let Some(status) = final_status {
                store
                    .mark_dispatching(&receipt.invocation_id, now())
                    .unwrap();
                let finalization = match status {
                    ReceiptFinalStatus::Succeeded => ReceiptFinalizationV1 {
                        status,
                        actual_cost_micro_usd: 3,
                        result_sha256: Some("b".repeat(64)),
                        result_storage_mode: Some(ResultStorageMode::DigestOnly),
                        result_envelope: None,
                        error_class: None,
                    },
                    ReceiptFinalStatus::OutcomeUnknown => ReceiptFinalizationV1 {
                        status,
                        actual_cost_micro_usd: 3,
                        result_sha256: None,
                        result_storage_mode: None,
                        result_envelope: None,
                        error_class: Some("unknown".into()),
                    },
                    ReceiptFinalStatus::Failed => unreachable!(),
                };
                store
                    .finalize(&receipt.invocation_id, finalization, now())
                    .unwrap();
            }

            let retry = match store
                .reserve_cost(request, now() + Duration::seconds(2))
                .unwrap()
            {
                RuntimeCostReservationOutcomeV1::Reserved { reservation } => reservation,
                other => panic!("exact retry must reuse authority: {other:?}"),
            };
            assert_eq!(retry.cost_ref, original.cost_ref);
            let outcome = store
                .admit(
                    context(approval_ref, retry.cost_ref),
                    now() + Duration::seconds(2),
                )
                .unwrap();
            match final_status {
                None => assert!(matches!(outcome, StoreAdmission::InProgress { .. })),
                Some(ReceiptFinalStatus::Succeeded) => {
                    assert!(matches!(outcome, StoreAdmission::Replay { .. }))
                }
                Some(ReceiptFinalStatus::OutcomeUnknown) => {
                    assert!(matches!(outcome, StoreAdmission::OutcomeUnknown { .. }))
                }
                Some(ReceiptFinalStatus::Failed) => unreachable!(),
            }
            assert_eq!(store.receipt_count().unwrap(), 1);
        }
    }

    #[test]
    fn new_reservation_after_approval_expiry_is_denied() {
        let store = test_store();
        let mut approval = approval_request(3);
        approval.ttl_seconds = 1;
        let approval_ref = match store.request_plan_approval(approval, true, now()).unwrap() {
            RuntimePlanApprovalOutcomeV1::Approved { approval_ref, .. } => approval_ref,
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(
            store
                .reserve_cost(
                    reserve_request(approval_ref, "new-after-expiry", 1),
                    now() + Duration::seconds(2),
                )
                .unwrap(),
            RuntimeCostReservationOutcomeV1::Denied { reason_code }
                if reason_code == "approval_expired"
        ));
        assert_eq!(store.count("cost_reservations").unwrap(), 0);
        assert_eq!(store.receipt_count().unwrap(), 0);
    }
}
