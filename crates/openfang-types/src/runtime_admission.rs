//! Transport contracts for runtime admission.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// The schema version accepted by runtime-admission contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAuthorityContractVersion {
    V1,
}

/// A request to approve the bounded cost of a planned runtime operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePlanApprovalRequestV1 {
    pub contract_version: RuntimeAuthorityContractVersion,
    pub correlation_id: String,
    pub plan_id: String,
    pub plan_revision: u64,
    pub space_id: String,
    pub agent_id: String,
    pub max_plan_cost_microusd: u64,
    pub ttl_seconds: u64,
}

impl RuntimePlanApprovalRequestV1 {
    /// Validate the request identity and positive lifetime bounds.
    pub fn validate(&self) -> Result<(), String> {
        validate_nonblank_fields(&[
            ("correlation_id", &self.correlation_id),
            ("plan_id", &self.plan_id),
            ("space_id", &self.space_id),
            ("agent_id", &self.agent_id),
        ])?;
        validate_positive("plan_revision", self.plan_revision)?;
        validate_positive("ttl_seconds", self.ttl_seconds)?;
        checked_ttl_duration(self.ttl_seconds).map(|_| ())
    }
}

/// The closed outcomes of a runtime plan-approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimePlanApprovalOutcomeV1 {
    Denied {
        reason_code: String,
    },
    PendingApproval {
        approval_ref: String,
    },
    Approved {
        approval_ref: String,
        expires_at: DateTime<Utc>,
    },
}

impl RuntimePlanApprovalOutcomeV1 {
    /// Validate the server-issued authority reference or denial reason.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Denied { reason_code } => validate_nonblank("reason_code", reason_code),
            Self::PendingApproval { approval_ref } | Self::Approved { approval_ref, .. } => {
                validate_nonblank("approval_ref", approval_ref)
            }
        }
    }
}

/// A request to reserve the bounded cost of one exact invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCostReservationRequestV1 {
    pub contract_version: RuntimeAuthorityContractVersion,
    pub correlation_id: String,
    pub plan_id: String,
    pub plan_revision: u64,
    pub space_id: String,
    pub agent_id: String,
    pub approval_ref: String,
    pub invocation_id: String,
    pub max_cost_microusd: u64,
    pub ttl_seconds: u64,
}

impl RuntimeCostReservationRequestV1 {
    /// Validate request identity, exact invocation binding, and lifetime bounds.
    pub fn validate(&self) -> Result<(), String> {
        validate_nonblank_fields(&[
            ("correlation_id", &self.correlation_id),
            ("plan_id", &self.plan_id),
            ("space_id", &self.space_id),
            ("agent_id", &self.agent_id),
            ("approval_ref", &self.approval_ref),
            ("invocation_id", &self.invocation_id),
        ])?;
        validate_positive("plan_revision", self.plan_revision)?;
        validate_positive("ttl_seconds", self.ttl_seconds)?;
        checked_ttl_duration(self.ttl_seconds).map(|_| ())
    }
}

/// Execution-bound context required before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAdmissionContextV1 {
    pub invocation_id: String,
    pub caller_agent_id: String,
    pub approval_ref: String,
    pub cost_ref: String,
    pub tool_name: String,
    pub arguments_sha256: String,
}

impl RuntimeAdmissionContextV1 {
    /// Validate nonblank authority references and the argument digest shape.
    pub fn validate(&self) -> Result<(), String> {
        validate_nonblank_fields(&[
            ("invocation_id", &self.invocation_id),
            ("caller_agent_id", &self.caller_agent_id),
            ("approval_ref", &self.approval_ref),
            ("cost_ref", &self.cost_ref),
            ("tool_name", &self.tool_name),
        ])?;

        if !is_sha256_hex(&self.arguments_sha256) {
            return Err("arguments_sha256 must be exactly 64 ASCII hex characters".into());
        }

        Ok(())
    }
}

/// Closed receipt states retained for a dispatch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionReceiptStatus {
    Prepared,
    Dispatching,
    Succeeded,
    Failed,
    OutcomeUnknown,
}

/// Closed result-retention modes for a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStorageMode {
    RedactedEnvelope,
    DigestOnly,
}

/// An OpenFang-issued reservation record bound to one invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCostReservationRecordV1 {
    pub cost_ref: String,
    pub approval_ref: String,
    pub invocation_id: String,
    pub correlation_id: String,
    pub plan_id: String,
    pub plan_revision: u64,
    pub space_id: String,
    pub agent_id: String,
    pub reserved_micro_usd: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl RuntimeCostReservationRecordV1 {
    /// Validate the structural integrity of a server-issued reservation record.
    pub fn validate(&self) -> Result<(), String> {
        validate_nonblank_fields(&[
            ("cost_ref", &self.cost_ref),
            ("approval_ref", &self.approval_ref),
            ("invocation_id", &self.invocation_id),
            ("correlation_id", &self.correlation_id),
            ("plan_id", &self.plan_id),
            ("space_id", &self.space_id),
            ("agent_id", &self.agent_id),
        ])?;
        validate_positive("plan_revision", self.plan_revision)?;
        if self.issued_at >= self.expires_at {
            return Err("issued_at must be before expires_at".into());
        }
        Ok(())
    }

    /// Validate this record and require exact binding to its reservation request.
    pub fn validate_against(
        &self,
        request: &RuntimeCostReservationRequestV1,
    ) -> Result<(), String> {
        request.validate()?;
        self.validate()?;

        for (name, record_value, request_value) in [
            ("approval_ref", &self.approval_ref, &request.approval_ref),
            ("invocation_id", &self.invocation_id, &request.invocation_id),
            (
                "correlation_id",
                &self.correlation_id,
                &request.correlation_id,
            ),
            ("plan_id", &self.plan_id, &request.plan_id),
            ("space_id", &self.space_id, &request.space_id),
            ("agent_id", &self.agent_id, &request.agent_id),
        ] {
            if record_value != request_value {
                return Err(format!("{name} must match request"));
            }
        }

        if self.plan_revision != request.plan_revision {
            return Err("plan_revision must match request".into());
        }
        if self.reserved_micro_usd != request.max_cost_microusd {
            return Err("reserved_micro_usd must match request max_cost_microusd".into());
        }

        let expected_expires_at = self
            .issued_at
            .checked_add_signed(checked_ttl_duration(request.ttl_seconds)?)
            .ok_or_else(|| "ttl_seconds produces an unrepresentable expires_at".to_string())?;
        if self.expires_at != expected_expires_at {
            return Err("expires_at must exactly match issued_at plus ttl_seconds".into());
        }

        Ok(())
    }
}

/// The closed result of a runtime cost-reservation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeCostReservationOutcomeV1 {
    Denied {
        reason_code: String,
    },
    Reserved {
        reservation: RuntimeCostReservationRecordV1,
    },
}

impl RuntimeCostReservationOutcomeV1 {
    /// Validate a closed cost-reservation outcome without an originating request.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Denied { reason_code } => validate_nonblank("reason_code", reason_code),
            Self::Reserved { reservation } => reservation.validate(),
        }
    }

    /// Validate a reserved outcome and require exact binding to its request.
    pub fn validate_against(
        &self,
        request: &RuntimeCostReservationRequestV1,
    ) -> Result<(), String> {
        request.validate()?;
        match self {
            Self::Denied { .. } => self.validate(),
            Self::Reserved { reservation } => reservation.validate_against(request),
        }
    }
}

/// An execution receipt for an admission-controlled invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReceiptV1 {
    pub invocation_id: String,
    pub correlation_id: String,
    pub plan_id: String,
    pub plan_revision: u64,
    pub space_id: String,
    pub agent_id: String,
    pub tool_name: String,
    pub arguments_sha256: String,
    pub approval_ref: String,
    pub cost_ref: String,
    pub status: ExecutionReceiptStatus,
    pub result_sha256: Option<String>,
    pub result_storage_mode: Option<ResultStorageMode>,
    pub result_envelope: Option<String>,
    pub error_class: Option<String>,
    pub prepared_at: DateTime<Utc>,
    pub dispatch_started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl ExecutionReceiptV1 {
    /// Validate receipt identity and state-dependent result retention fields.
    pub fn validate(&self, receipt_max_bytes: usize) -> Result<(), String> {
        validate_nonblank_fields(&[
            ("invocation_id", &self.invocation_id),
            ("correlation_id", &self.correlation_id),
            ("plan_id", &self.plan_id),
            ("space_id", &self.space_id),
            ("agent_id", &self.agent_id),
            ("tool_name", &self.tool_name),
            ("approval_ref", &self.approval_ref),
            ("cost_ref", &self.cost_ref),
        ])?;
        validate_positive("plan_revision", self.plan_revision)?;
        validate_sha256("arguments_sha256", &self.arguments_sha256)?;

        match self.status {
            ExecutionReceiptStatus::Prepared => {
                require_absent("dispatch_started_at", &self.dispatch_started_at)?;
                require_absent("finished_at", &self.finished_at)?;
                validate_absent_result_fields(self)?;
                require_absent("error_class", &self.error_class)
            }
            ExecutionReceiptStatus::Dispatching => {
                validate_started_at(self)?;
                require_absent("finished_at", &self.finished_at)?;
                validate_absent_result_fields(self)?;
                require_absent("error_class", &self.error_class)
            }
            ExecutionReceiptStatus::Succeeded => {
                validate_terminal_timestamps(self)?;
                validate_required_result_fields(
                    &self.result_sha256,
                    &self.result_storage_mode,
                    &self.result_envelope,
                    receipt_max_bytes,
                )?;
                require_absent("error_class", &self.error_class)
            }
            ExecutionReceiptStatus::Failed | ExecutionReceiptStatus::OutcomeUnknown => {
                validate_terminal_timestamps(self)?;
                validate_absent_result_fields(self)?;
                validate_optional_nonblank("error_class", self.error_class.as_deref())
            }
        }
    }
}

/// The closed set of runtime-admission decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeAdmissionDecisionV1 {
    Denied { reason_code: String },
    PendingApproval { approval_ref: String },
    CostUnavailable { reason_code: String },
    Admitted { receipt: ExecutionReceiptV1 },
    InProgress { receipt: ExecutionReceiptV1 },
    Replay { receipt: ExecutionReceiptV1 },
    OutcomeUnknown { receipt: ExecutionReceiptV1 },
}

impl RuntimeAdmissionDecisionV1 {
    /// Validate the denial, authority reference, or receipt carried by this decision.
    pub fn validate(&self, receipt_max_bytes: usize) -> Result<(), String> {
        match self {
            Self::Denied { reason_code } | Self::CostUnavailable { reason_code } => {
                validate_nonblank("reason_code", reason_code)
            }
            Self::PendingApproval { approval_ref } => {
                validate_nonblank("approval_ref", approval_ref)
            }
            Self::Admitted { receipt } => {
                receipt.validate(receipt_max_bytes)?;
                validate_receipt_status(receipt, ExecutionReceiptStatus::Prepared, "admitted")
            }
            Self::InProgress { receipt } => {
                receipt.validate(receipt_max_bytes)?;
                if matches!(
                    receipt.status,
                    ExecutionReceiptStatus::Prepared | ExecutionReceiptStatus::Dispatching
                ) {
                    Ok(())
                } else {
                    Err("receipt.status must be prepared or dispatching for in_progress".into())
                }
            }
            Self::Replay { receipt } => {
                receipt.validate(receipt_max_bytes)?;
                if matches!(
                    receipt.status,
                    ExecutionReceiptStatus::Succeeded | ExecutionReceiptStatus::Failed
                ) {
                    Ok(())
                } else {
                    Err("receipt.status must be succeeded or failed for replay".into())
                }
            }
            Self::OutcomeUnknown { receipt } => {
                receipt.validate(receipt_max_bytes)?;
                validate_receipt_status(
                    receipt,
                    ExecutionReceiptStatus::OutcomeUnknown,
                    "outcome_unknown",
                )
            }
        }
    }
}

/// The only final receipt states accepted at finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptFinalStatus {
    Succeeded,
    Failed,
    OutcomeUnknown,
}

/// Final outcome data recorded after a dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptFinalizationV1 {
    pub status: ReceiptFinalStatus,
    pub actual_cost_micro_usd: u64,
    pub result_sha256: Option<String>,
    pub result_storage_mode: Option<ResultStorageMode>,
    pub result_envelope: Option<String>,
    pub error_class: Option<String>,
}

impl ReceiptFinalizationV1 {
    /// Validate terminal result retention without assuming persistence behavior.
    pub fn validate(&self, receipt_max_bytes: usize) -> Result<(), String> {
        match self.status {
            ReceiptFinalStatus::Succeeded => {
                validate_required_result_fields(
                    &self.result_sha256,
                    &self.result_storage_mode,
                    &self.result_envelope,
                    receipt_max_bytes,
                )?;
                require_absent("error_class", &self.error_class)
            }
            ReceiptFinalStatus::Failed | ReceiptFinalStatus::OutcomeUnknown => {
                validate_absent_finalization_result_fields(self)?;
                validate_optional_nonblank("error_class", self.error_class.as_deref())
            }
        }
    }

    /// Validate this finalization against its issued reservation ceiling.
    pub fn validate_against(
        &self,
        reservation: &RuntimeCostReservationRecordV1,
        receipt_max_bytes: usize,
    ) -> Result<(), String> {
        reservation.validate()?;
        self.validate(receipt_max_bytes)?;
        if self.actual_cost_micro_usd > reservation.reserved_micro_usd {
            return Err("actual_cost_micro_usd must not exceed reserved_micro_usd".into());
        }
        Ok(())
    }
}

fn validate_nonblank_fields(fields: &[(&str, &String)]) -> Result<(), String> {
    for (name, value) in fields {
        if value.trim().is_empty() {
            return Err(format!("{name} must not be empty"));
        }
    }
    Ok(())
}

fn validate_nonblank(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    Ok(())
}

fn validate_optional_nonblank(name: &str, value: Option<&str>) -> Result<(), String> {
    validate_nonblank(name, value.ok_or_else(|| format!("{name} is required"))?)
}

fn require_absent<T>(name: &str, value: &Option<T>) -> Result<(), String> {
    if value.is_some() {
        return Err(format!("{name} must be absent"));
    }
    Ok(())
}

fn validate_sha256(name: &str, value: &str) -> Result<(), String> {
    if !is_sha256_hex(value) {
        return Err(format!("{name} must be exactly 64 ASCII hex characters"));
    }
    Ok(())
}

fn validate_started_at(receipt: &ExecutionReceiptV1) -> Result<(), String> {
    let dispatch_started_at = receipt
        .dispatch_started_at
        .ok_or_else(|| "dispatch_started_at is required".to_string())?;
    if dispatch_started_at < receipt.prepared_at {
        return Err("dispatch_started_at must not precede prepared_at".into());
    }
    Ok(())
}

fn validate_terminal_timestamps(receipt: &ExecutionReceiptV1) -> Result<(), String> {
    validate_started_at(receipt)?;
    let dispatch_started_at = receipt.dispatch_started_at.expect("validated above");
    let finished_at = receipt
        .finished_at
        .ok_or_else(|| "finished_at is required".to_string())?;
    if finished_at < dispatch_started_at {
        return Err("finished_at must not precede dispatch_started_at".into());
    }
    Ok(())
}

fn validate_receipt_status(
    receipt: &ExecutionReceiptV1,
    expected: ExecutionReceiptStatus,
    decision_status: &str,
) -> Result<(), String> {
    if receipt.status != expected {
        return Err(format!(
            "receipt.status must be {expected:?} for {decision_status}"
        ));
    }
    Ok(())
}

fn validate_absent_result_fields(receipt: &ExecutionReceiptV1) -> Result<(), String> {
    require_absent("result_sha256", &receipt.result_sha256)?;
    require_absent("result_storage_mode", &receipt.result_storage_mode)?;
    require_absent("result_envelope", &receipt.result_envelope)
}

fn validate_absent_finalization_result_fields(
    finalization: &ReceiptFinalizationV1,
) -> Result<(), String> {
    require_absent("result_sha256", &finalization.result_sha256)?;
    require_absent("result_storage_mode", &finalization.result_storage_mode)?;
    require_absent("result_envelope", &finalization.result_envelope)
}

fn validate_required_result_fields(
    result_sha256: &Option<String>,
    result_storage_mode: &Option<ResultStorageMode>,
    result_envelope: &Option<String>,
    receipt_max_bytes: usize,
) -> Result<(), String> {
    if receipt_max_bytes == 0 {
        return Err("receipt_max_bytes must be positive".into());
    }
    validate_sha256(
        "result_sha256",
        result_sha256
            .as_deref()
            .ok_or_else(|| "result_sha256 is required".to_string())?,
    )?;
    match result_storage_mode
        .as_ref()
        .ok_or_else(|| "result_storage_mode is required".to_string())?
    {
        ResultStorageMode::RedactedEnvelope => {
            let envelope = result_envelope
                .as_ref()
                .ok_or_else(|| "result_envelope is required for redacted_envelope".to_string())?;
            if envelope.len() > receipt_max_bytes {
                return Err("result_envelope exceeds receipt_max_bytes".into());
            }
            Ok(())
        }
        ResultStorageMode::DigestOnly => require_absent("result_envelope", result_envelope),
    }
}

fn validate_positive(name: &str, value: u64) -> Result<(), String> {
    if value == 0 {
        return Err(format!("{name} must be positive"));
    }
    Ok(())
}

fn checked_ttl_duration(ttl_seconds: u64) -> Result<Duration, String> {
    let seconds = i64::try_from(ttl_seconds)
        .map_err(|_| "ttl_seconds must fit in a signed duration".to_string())?;
    Duration::try_seconds(seconds)
        .ok_or_else(|| "ttl_seconds cannot be represented as a duration".to_string())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use serde_json::json;

    fn valid_reservation_request() -> RuntimeCostReservationRequestV1 {
        RuntimeCostReservationRequestV1 {
            contract_version: RuntimeAuthorityContractVersion::V1,
            correlation_id: "corr-1".into(),
            plan_id: "plan-1".into(),
            plan_revision: 1,
            space_id: "ideas".into(),
            agent_id: "00000000-0000-0000-0000-000000000001".into(),
            approval_ref: "approval:plan-1".into(),
            invocation_id: "brain:plan-1:1:step-1".into(),
            max_cost_microusd: 0,
            ttl_seconds: 300,
        }
    }

    fn valid_plan_approval_request() -> RuntimePlanApprovalRequestV1 {
        RuntimePlanApprovalRequestV1 {
            contract_version: RuntimeAuthorityContractVersion::V1,
            correlation_id: "corr-1".into(),
            plan_id: "plan-1".into(),
            plan_revision: 1,
            space_id: "ideas".into(),
            agent_id: "00000000-0000-0000-0000-000000000001".into(),
            max_plan_cost_microusd: 0,
            ttl_seconds: 300,
        }
    }

    fn test_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap()
    }

    fn valid_reservation_record(
        request: &RuntimeCostReservationRequestV1,
    ) -> RuntimeCostReservationRecordV1 {
        let issued_at = test_time();
        RuntimeCostReservationRecordV1 {
            cost_ref: "cost:plan-1".into(),
            approval_ref: request.approval_ref.clone(),
            invocation_id: request.invocation_id.clone(),
            correlation_id: request.correlation_id.clone(),
            plan_id: request.plan_id.clone(),
            plan_revision: request.plan_revision,
            space_id: request.space_id.clone(),
            agent_id: request.agent_id.clone(),
            reserved_micro_usd: request.max_cost_microusd,
            issued_at,
            expires_at: issued_at + Duration::seconds(request.ttl_seconds as i64),
        }
    }

    fn succeeded_receipt() -> ExecutionReceiptV1 {
        let prepared_at = test_time();
        let dispatch_started_at = prepared_at + Duration::seconds(1);
        ExecutionReceiptV1 {
            invocation_id: "brain:plan-1:1:step-1".into(),
            correlation_id: "corr-1".into(),
            plan_id: "plan-1".into(),
            plan_revision: 1,
            space_id: "ideas".into(),
            agent_id: "00000000-0000-0000-0000-000000000001".into(),
            tool_name: "notes.search".into(),
            arguments_sha256: "a".repeat(64),
            approval_ref: "approval:plan-1".into(),
            cost_ref: "cost:plan-1".into(),
            status: ExecutionReceiptStatus::Succeeded,
            result_sha256: Some("b".repeat(64)),
            result_storage_mode: Some(ResultStorageMode::RedactedEnvelope),
            result_envelope: Some("redacted".into()),
            error_class: None,
            prepared_at,
            dispatch_started_at: Some(dispatch_started_at),
            finished_at: Some(dispatch_started_at + Duration::seconds(1)),
        }
    }

    fn receipt_with_status(status: ExecutionReceiptStatus) -> ExecutionReceiptV1 {
        let mut receipt = succeeded_receipt();
        receipt.status = status;
        match status {
            ExecutionReceiptStatus::Prepared => {
                receipt.result_sha256 = None;
                receipt.result_storage_mode = None;
                receipt.result_envelope = None;
                receipt.dispatch_started_at = None;
                receipt.finished_at = None;
            }
            ExecutionReceiptStatus::Dispatching => {
                receipt.result_sha256 = None;
                receipt.result_storage_mode = None;
                receipt.result_envelope = None;
                receipt.finished_at = None;
            }
            ExecutionReceiptStatus::Succeeded => {}
            ExecutionReceiptStatus::Failed | ExecutionReceiptStatus::OutcomeUnknown => {
                receipt.result_sha256 = None;
                receipt.result_storage_mode = None;
                receipt.result_envelope = None;
                receipt.error_class = Some("transport_failure".into());
            }
        }
        receipt
    }

    #[test]
    fn deterministic_zero_cost_reservation_is_valid_and_invocation_bound() {
        let request = valid_reservation_request();

        assert_eq!(request.invocation_id, "brain:plan-1:1:step-1");
        assert!(request.validate().is_ok());
    }

    #[test]
    fn requests_reject_unrepresentable_ttl_without_panicking() {
        let mut plan_approval = valid_plan_approval_request();
        plan_approval.ttl_seconds = u64::MAX;
        let plan_result = std::panic::catch_unwind(|| plan_approval.validate());
        assert!(
            plan_result.is_ok(),
            "plan approval validation must not panic"
        );
        assert!(plan_result.unwrap().unwrap_err().contains("ttl_seconds"));

        let mut reservation = valid_reservation_request();
        reservation.ttl_seconds = u64::MAX;
        let reservation_result = std::panic::catch_unwind(|| reservation.validate());
        assert!(
            reservation_result.is_ok(),
            "reservation validation must not panic"
        );
        assert!(reservation_result
            .unwrap()
            .unwrap_err()
            .contains("ttl_seconds"));
    }

    #[test]
    fn admission_context_rejects_blank_refs_and_non_sha256_arguments_digest() {
        let context = RuntimeAdmissionContextV1 {
            invocation_id: "brain:plan-1:1:step-1".into(),
            caller_agent_id: "00000000-0000-0000-0000-000000000001".into(),
            approval_ref: "approval:plan-1".into(),
            cost_ref: " ".into(),
            tool_name: "notes.search".into(),
            arguments_sha256: "abc".into(),
        };

        let error = context.validate().unwrap_err();
        assert!(error.contains("cost_ref"));

        let mut invalid_digest = context;
        invalid_digest.cost_ref = "cost:plan-1".into();
        let error = invalid_digest.validate().unwrap_err();
        assert!(error.contains("arguments_sha256"));
    }

    #[test]
    fn receipt_status_and_result_storage_modes_are_closed_enums() {
        let prepared: ExecutionReceiptStatus = serde_json::from_value(json!("prepared")).unwrap();
        assert_eq!(prepared, ExecutionReceiptStatus::Prepared);

        let retrying: Result<ExecutionReceiptStatus, _> = serde_json::from_value(json!("retrying"));
        assert!(retrying.is_err());

        let digest_only: ResultStorageMode = serde_json::from_value(json!("digest_only")).unwrap();
        assert_eq!(digest_only, ResultStorageMode::DigestOnly);
    }

    #[test]
    fn runtime_admission_decisions_are_payload_bound_and_reject_unknown_fields() {
        let receipt = json!({
            "invocation_id": "brain:plan-1:1:step-1",
            "correlation_id": "corr-1",
            "plan_id": "plan-1",
            "plan_revision": 1,
            "space_id": "ideas",
            "agent_id": "00000000-0000-0000-0000-000000000001",
            "tool_name": "notes.search",
            "arguments_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "approval_ref": "approval:plan-1",
            "cost_ref": "cost:plan-1",
            "status": "prepared",
            "result_sha256": null,
            "result_storage_mode": null,
            "result_envelope": null,
            "error_class": null,
            "prepared_at": "2026-01-01T00:00:00Z",
            "dispatch_started_at": null,
            "finished_at": null
        });
        let cases = vec![
            json!({"status": "denied", "reason_code": "policy_denied"}),
            json!({"status": "pending_approval", "approval_ref": "approval:plan-1"}),
            json!({"status": "cost_unavailable", "reason_code": "budget_unavailable"}),
            json!({"status": "admitted", "receipt": receipt}),
            json!({"status": "in_progress", "receipt": receipt}),
            json!({"status": "replay", "receipt": receipt}),
            json!({"status": "outcome_unknown", "receipt": receipt}),
        ];

        for value in cases {
            let parsed: RuntimeAdmissionDecisionV1 = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), value);

            let mut with_unknown = value;
            with_unknown["unexpected"] = json!(true);
            let rejected: Result<RuntimeAdmissionDecisionV1, _> =
                serde_json::from_value(with_unknown);
            assert!(rejected.is_err(), "extra decision fields must be rejected");
        }
    }

    #[test]
    fn server_issued_artifacts_validate_bindings_and_receipt_state() {
        let denied = RuntimePlanApprovalOutcomeV1::Denied {
            reason_code: "policy_denied".into(),
        };
        assert!(denied.validate().is_ok());
        let blank_denied = RuntimePlanApprovalOutcomeV1::Denied {
            reason_code: " ".into(),
        };
        assert!(blank_denied.validate().unwrap_err().contains("reason_code"));

        let request = valid_reservation_request();
        let record = valid_reservation_record(&request);
        assert!(record.validate().is_ok());
        assert!(record.validate_against(&request).is_ok());

        let mut wrong_invocation = record.clone();
        wrong_invocation.invocation_id = "brain:plan-1:1:step-2".into();
        assert!(wrong_invocation
            .validate_against(&request)
            .unwrap_err()
            .contains("invocation_id"));

        let mut wrong_expiry = record.clone();
        wrong_expiry.expires_at += Duration::seconds(1);
        assert!(wrong_expiry
            .validate_against(&request)
            .unwrap_err()
            .contains("expires_at"));

        let reservation_outcome = RuntimeCostReservationOutcomeV1::Reserved {
            reservation: record.clone(),
        };
        assert!(reservation_outcome.validate_against(&request).is_ok());
        let blank_cost_denial = RuntimeCostReservationOutcomeV1::Denied {
            reason_code: "\t".into(),
        };
        assert!(blank_cost_denial
            .validate()
            .unwrap_err()
            .contains("reason_code"));

        let receipt = succeeded_receipt();
        assert!(receipt.validate(16).is_ok());

        let mut digest_only = receipt.clone();
        digest_only.result_storage_mode = Some(ResultStorageMode::DigestOnly);
        digest_only.result_envelope = None;
        assert!(digest_only.validate(16).is_ok());
        digest_only.result_envelope = Some("must-not-be-stored".into());
        assert!(digest_only
            .validate(16)
            .unwrap_err()
            .contains("result_envelope"));

        let mut oversized = receipt.clone();
        oversized.result_envelope = Some("too-large".repeat(4));
        assert!(oversized
            .validate(16)
            .unwrap_err()
            .contains("receipt_max_bytes"));

        let admitted = RuntimeAdmissionDecisionV1::Admitted {
            receipt: receipt_with_status(ExecutionReceiptStatus::Prepared),
        };
        assert!(admitted.validate(16).is_ok());
        let blank_pending = RuntimeAdmissionDecisionV1::PendingApproval {
            approval_ref: " ".into(),
        };
        assert!(blank_pending
            .validate(16)
            .unwrap_err()
            .contains("approval_ref"));

        let finalization = ReceiptFinalizationV1 {
            status: ReceiptFinalStatus::Succeeded,
            actual_cost_micro_usd: 0,
            result_sha256: Some("b".repeat(64)),
            result_storage_mode: Some(ResultStorageMode::RedactedEnvelope),
            result_envelope: Some("redacted".into()),
            error_class: None,
        };
        assert!(finalization.validate(16).is_ok());

        let invalid_finalization = ReceiptFinalizationV1 {
            result_storage_mode: Some(ResultStorageMode::DigestOnly),
            result_envelope: Some("must-not-be-stored".into()),
            ..finalization
        };
        assert!(invalid_finalization
            .validate(16)
            .unwrap_err()
            .contains("result_envelope"));
    }

    #[test]
    fn runtime_admission_decision_enforces_receipt_status_matrix() {
        let admitted = RuntimeAdmissionDecisionV1::Admitted {
            receipt: receipt_with_status(ExecutionReceiptStatus::Prepared),
        };
        assert!(admitted.validate(16).is_ok());

        let in_progress = RuntimeAdmissionDecisionV1::InProgress {
            receipt: receipt_with_status(ExecutionReceiptStatus::Dispatching),
        };
        assert!(in_progress.validate(16).is_ok());

        let prepared_in_progress = RuntimeAdmissionDecisionV1::InProgress {
            receipt: receipt_with_status(ExecutionReceiptStatus::Prepared),
        };
        assert!(prepared_in_progress.validate(16).is_ok());

        let replay_succeeded = RuntimeAdmissionDecisionV1::Replay {
            receipt: receipt_with_status(ExecutionReceiptStatus::Succeeded),
        };
        assert!(replay_succeeded.validate(16).is_ok());
        let replay_failed = RuntimeAdmissionDecisionV1::Replay {
            receipt: receipt_with_status(ExecutionReceiptStatus::Failed),
        };
        assert!(replay_failed.validate(16).is_ok());

        let outcome_unknown = RuntimeAdmissionDecisionV1::OutcomeUnknown {
            receipt: receipt_with_status(ExecutionReceiptStatus::OutcomeUnknown),
        };
        assert!(outcome_unknown.validate(16).is_ok());

        let wrong_admitted = RuntimeAdmissionDecisionV1::Admitted {
            receipt: receipt_with_status(ExecutionReceiptStatus::Succeeded),
        };
        assert!(wrong_admitted
            .validate(16)
            .unwrap_err()
            .contains("receipt.status"));

        let wrong_in_progress = RuntimeAdmissionDecisionV1::InProgress {
            receipt: receipt_with_status(ExecutionReceiptStatus::Succeeded),
        };
        assert!(wrong_in_progress
            .validate(16)
            .unwrap_err()
            .contains("receipt.status"));

        let wrong_replay = RuntimeAdmissionDecisionV1::Replay {
            receipt: receipt_with_status(ExecutionReceiptStatus::Dispatching),
        };
        assert!(wrong_replay
            .validate(16)
            .unwrap_err()
            .contains("receipt.status"));

        let wrong_outcome_unknown = RuntimeAdmissionDecisionV1::OutcomeUnknown {
            receipt: receipt_with_status(ExecutionReceiptStatus::Failed),
        };
        assert!(wrong_outcome_unknown
            .validate(16)
            .unwrap_err()
            .contains("receipt.status"));
    }

    #[test]
    fn finalization_cannot_exceed_its_reservation() {
        let mut request = valid_reservation_request();
        request.max_cost_microusd = 10;
        let reservation = valid_reservation_record(&request);
        let base = ReceiptFinalizationV1 {
            status: ReceiptFinalStatus::Succeeded,
            actual_cost_micro_usd: 0,
            result_sha256: Some("b".repeat(64)),
            result_storage_mode: Some(ResultStorageMode::DigestOnly),
            result_envelope: None,
            error_class: None,
        };
        assert!(base.validate_against(&reservation, 16).is_ok());

        let equal = ReceiptFinalizationV1 {
            actual_cost_micro_usd: reservation.reserved_micro_usd,
            ..base.clone()
        };
        assert!(equal.validate_against(&reservation, 16).is_ok());

        let exceeds = ReceiptFinalizationV1 {
            actual_cost_micro_usd: reservation.reserved_micro_usd + 1,
            ..base
        };
        assert!(exceeds
            .validate_against(&reservation, 16)
            .unwrap_err()
            .contains("actual_cost_micro_usd"));
    }

    #[test]
    fn denied_cost_outcome_still_rejects_an_invalid_request() {
        let mut invalid_request = valid_reservation_request();
        invalid_request.ttl_seconds = 0;
        let denied = RuntimeCostReservationOutcomeV1::Denied {
            reason_code: "budget_unavailable".into(),
        };

        assert!(denied
            .validate_against(&invalid_request)
            .unwrap_err()
            .contains("ttl_seconds"));
    }
}
