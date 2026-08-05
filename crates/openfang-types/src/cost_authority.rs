//! Fail-closed transport types for OpenFang cost-authority reservations.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// The schema version accepted by the cost-authority reservation contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostAuthorityContractVersion {
    V1,
}

/// A versioned request to reserve a bounded OpenFang cost authority.
///
/// Amounts are expressed only as integer micro-USD. `approval_ref` is an
/// opaque reference issued by OpenFang; this contract does not mint or verify
/// that reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostReservationRequestV1 {
    pub contract_version: CostAuthorityContractVersion,
    pub correlation_id: String,
    pub plan_id: String,
    pub plan_revision: u64,
    pub space_id: String,
    pub agent_id: String,
    pub approval_ref: String,
    pub max_cost_microusd: u64,
    pub ttl_seconds: u64,
}

impl CostReservationRequestV1 {
    /// Validate the structural identity and positive bounds of a request.
    pub fn validate(&self) -> Result<(), String> {
        validate_nonblank_fields(&[
            ("correlation_id", &self.correlation_id),
            ("plan_id", &self.plan_id),
            ("space_id", &self.space_id),
            ("agent_id", &self.agent_id),
            ("approval_ref", &self.approval_ref),
        ])?;

        validate_positive("plan_revision", self.plan_revision)?;
        validate_positive("max_cost_microusd", self.max_cost_microusd)?;
        validate_positive("ttl_seconds", self.ttl_seconds)
    }
}

/// An OpenFang-issued, request-bound cost reservation record.
///
/// This data is a transport record only. It neither reserves quota itself nor
/// proves that the associated approval or later execution occurred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostReservationRecordV1 {
    pub contract_version: CostAuthorityContractVersion,
    pub cost_ref: String,
    pub correlation_id: String,
    pub plan_id: String,
    pub plan_revision: u64,
    pub space_id: String,
    pub agent_id: String,
    pub approval_ref: String,
    pub max_cost_microusd: u64,
    pub ttl_seconds: u64,
    pub reserved_cost_microusd: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl CostReservationRecordV1 {
    /// Validate a reservation record without a request to bind it against.
    pub fn validate(&self) -> Result<(), String> {
        validate_nonblank_fields(&[
            ("cost_ref", &self.cost_ref),
            ("correlation_id", &self.correlation_id),
            ("plan_id", &self.plan_id),
            ("space_id", &self.space_id),
            ("agent_id", &self.agent_id),
            ("approval_ref", &self.approval_ref),
        ])?;

        validate_positive("plan_revision", self.plan_revision)?;
        validate_positive("max_cost_microusd", self.max_cost_microusd)?;
        validate_positive("ttl_seconds", self.ttl_seconds)?;
        validate_positive("reserved_cost_microusd", self.reserved_cost_microusd)?;

        if self.expires_at <= self.issued_at {
            return Err("expires_at must be after issued_at".into());
        }

        if self.reserved_cost_microusd != self.max_cost_microusd {
            return Err("reserved_cost_microusd must exactly match max_cost_microusd".into());
        }

        let ttl_seconds = i64::try_from(self.ttl_seconds)
            .map_err(|_| "ttl_seconds is too large to represent".to_string())?;
        let expected_expires_at = self
            .issued_at
            .checked_add_signed(Duration::seconds(ttl_seconds))
            .ok_or_else(|| "ttl_seconds produces an unrepresentable expires_at".to_string())?;
        if self.expires_at != expected_expires_at {
            return Err("expires_at must exactly match issued_at plus ttl_seconds".into());
        }

        Ok(())
    }

    /// Validate this record and require exact binding to the reservation request.
    pub fn validate_against(&self, request: &CostReservationRequestV1) -> Result<(), String> {
        request.validate()?;
        self.validate()?;

        if self.contract_version != request.contract_version {
            return Err("contract_version must match request".into());
        }

        for (name, record_value, request_value) in [
            (
                "correlation_id",
                &self.correlation_id,
                &request.correlation_id,
            ),
            ("plan_id", &self.plan_id, &request.plan_id),
            ("space_id", &self.space_id, &request.space_id),
            ("agent_id", &self.agent_id, &request.agent_id),
            ("approval_ref", &self.approval_ref, &request.approval_ref),
        ] {
            if record_value != request_value {
                return Err(format!("{name} must match request"));
            }
        }

        for (name, record_value, request_value) in [
            ("plan_revision", self.plan_revision, request.plan_revision),
            (
                "max_cost_microusd",
                self.max_cost_microusd,
                request.max_cost_microusd,
            ),
            ("ttl_seconds", self.ttl_seconds, request.ttl_seconds),
        ] {
            if record_value != request_value {
                return Err(format!("{name} must match request"));
            }
        }

        if self.reserved_cost_microusd != request.max_cost_microusd {
            return Err(
                "reserved_cost_microusd must exactly match request max_cost_microusd".into(),
            );
        }

        Ok(())
    }
}

/// The closed set of cost-authority outcomes OpenFang may report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CostReservationOutcomeV1 {
    Denied,
    Reserved {
        reservation: CostReservationRecordV1,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum CostReservationOutcomeV1Wire {
    Denied {},
    Reserved {
        reservation: CostReservationRecordV1,
    },
}

impl<'de> Deserialize<'de> for CostReservationOutcomeV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match CostReservationOutcomeV1Wire::deserialize(deserializer)? {
            CostReservationOutcomeV1Wire::Denied {} => Ok(Self::Denied),
            CostReservationOutcomeV1Wire::Reserved { reservation } => {
                Ok(Self::Reserved { reservation })
            }
        }
    }
}

impl CostReservationOutcomeV1 {
    /// Validate the outcome's enclosed reservation, if one was issued.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Denied => Ok(()),
            Self::Reserved { reservation } => reservation.validate(),
        }
    }

    /// Validate this outcome against the request it responds to.
    pub fn validate_against(&self, request: &CostReservationRequestV1) -> Result<(), String> {
        request.validate()?;
        match self {
            Self::Denied => Ok(()),
            Self::Reserved { reservation } => reservation.validate_against(request),
        }
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

fn validate_positive(name: &str, value: u64) -> Result<(), String> {
    if value == 0 {
        Err(format!("{name} must be positive"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::json;

    fn valid_request() -> CostReservationRequestV1 {
        CostReservationRequestV1 {
            contract_version: CostAuthorityContractVersion::V1,
            correlation_id: "corr-001".into(),
            plan_id: "plan-001".into(),
            plan_revision: 1,
            space_id: "coding".into(),
            agent_id: "coding-agent".into(),
            approval_ref: "approval:issued-001".into(),
            max_cost_microusd: 125_000,
            ttl_seconds: 300,
        }
    }

    fn valid_reservation(request: &CostReservationRequestV1) -> CostReservationRecordV1 {
        let issued_at = Utc.with_ymd_and_hms(2026, 8, 5, 12, 0, 0).unwrap();
        CostReservationRecordV1 {
            contract_version: CostAuthorityContractVersion::V1,
            cost_ref: "cost:openfang-001".into(),
            correlation_id: request.correlation_id.clone(),
            plan_id: request.plan_id.clone(),
            plan_revision: request.plan_revision,
            space_id: request.space_id.clone(),
            agent_id: request.agent_id.clone(),
            approval_ref: request.approval_ref.clone(),
            max_cost_microusd: request.max_cost_microusd,
            ttl_seconds: request.ttl_seconds,
            reserved_cost_microusd: request.max_cost_microusd,
            issued_at,
            expires_at: issued_at + Duration::seconds(request.ttl_seconds as i64),
        }
    }

    #[test]
    fn reservation_round_trips_and_binds_to_its_request() {
        let request = valid_request();
        let reservation = valid_reservation(&request);
        let outcome = CostReservationOutcomeV1::Reserved { reservation };

        request.validate().unwrap();
        outcome.validate_against(&request).unwrap();

        let encoded = serde_json::to_string(&outcome).unwrap();
        let restored: CostReservationOutcomeV1 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, outcome);
    }

    #[test]
    fn request_rejects_unknown_fields_and_client_cost_ref() {
        let value = json!({
            "contract_version": "v1",
            "correlation_id": "corr-001",
            "plan_id": "plan-001",
            "plan_revision": 1,
            "space_id": "coding",
            "agent_id": "coding-agent",
            "approval_ref": "approval:issued-001",
            "max_cost_microusd": 125000,
            "ttl_seconds": 300,
            "cost_ref": "client-forged"
        });
        assert!(serde_json::from_value::<CostReservationRequestV1>(value).is_err());

        let value = json!({
            "contract_version": "v1",
            "correlation_id": "corr-001",
            "plan_id": "plan-001",
            "plan_revision": 1,
            "space_id": "coding",
            "agent_id": "coding-agent",
            "approval_ref": "approval:issued-001",
            "max_cost_microusd": 125000,
            "ttl_seconds": 300,
            "unexpected": true
        });
        assert!(serde_json::from_value::<CostReservationRequestV1>(value).is_err());
    }

    #[test]
    fn request_rejects_blank_or_zero_required_values() {
        for field in [
            "correlation_id",
            "plan_id",
            "space_id",
            "agent_id",
            "approval_ref",
        ] {
            let mut request = valid_request();
            match field {
                "correlation_id" => request.correlation_id = " \t ".into(),
                "plan_id" => request.plan_id = " \t ".into(),
                "space_id" => request.space_id = " \t ".into(),
                "agent_id" => request.agent_id = " \t ".into(),
                "approval_ref" => request.approval_ref = " \t ".into(),
                _ => unreachable!(),
            }
            assert!(request.validate().unwrap_err().contains(field));
        }

        let mut request = valid_request();
        request.plan_revision = 0;
        assert!(request.validate().unwrap_err().contains("plan_revision"));
        let mut request = valid_request();
        request.max_cost_microusd = 0;
        assert!(request
            .validate()
            .unwrap_err()
            .contains("max_cost_microusd"));
        let mut request = valid_request();
        request.ttl_seconds = 0;
        assert!(request.validate().unwrap_err().contains("ttl_seconds"));
    }

    #[test]
    fn request_json_rejects_negative_and_fractional_integer_values() {
        for (field, value) in [
            ("plan_revision", json!(-1)),
            ("max_cost_microusd", json!(-1)),
            ("ttl_seconds", json!(-1)),
            ("plan_revision", json!(1.5)),
            ("max_cost_microusd", json!(1.5)),
            ("ttl_seconds", json!(1.5)),
        ] {
            let mut request = serde_json::to_value(valid_request()).unwrap();
            request[field] = value;
            assert!(serde_json::from_value::<CostReservationRequestV1>(request).is_err());
        }
    }

    #[test]
    fn reservation_validation_rejects_blank_refs_zero_values_and_bad_timestamps() {
        let request = valid_request();
        let mut reservation = valid_reservation(&request);
        reservation.cost_ref = " \n ".into();
        assert!(reservation.validate().unwrap_err().contains("cost_ref"));

        let mut reservation = valid_reservation(&request);
        reservation.reserved_cost_microusd = 0;
        assert!(reservation
            .validate()
            .unwrap_err()
            .contains("reserved_cost_microusd"));

        let mut reservation = valid_reservation(&request);
        reservation.reserved_cost_microusd = 1;
        assert!(reservation
            .validate()
            .unwrap_err()
            .contains("exactly match"));

        let mut reservation = valid_reservation(&request);
        reservation.expires_at = reservation.issued_at;
        assert!(reservation.validate().unwrap_err().contains("expires_at"));
    }

    #[test]
    fn reservation_validation_rejects_expiry_inconsistent_with_ttl() {
        let request = valid_request();
        let mut reservation = valid_reservation(&request);
        reservation.expires_at += Duration::seconds(1);

        assert!(reservation.validate().unwrap_err().contains("ttl_seconds"));
    }

    #[test]
    fn reservation_binding_rejects_every_mismatch_and_amount_mismatch() {
        let request = valid_request();
        let mut reservation = valid_reservation(&request);
        reservation.correlation_id = "other".into();
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.plan_id = "other".into();
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.plan_revision = 2;
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.space_id = "other".into();
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.agent_id = "other".into();
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.approval_ref = "approval:other".into();
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.max_cost_microusd = 1;
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.ttl_seconds = 1;
        assert!(reservation.validate_against(&request).is_err());

        let mut reservation = valid_reservation(&request);
        reservation.reserved_cost_microusd = 1;
        assert!(reservation.validate_against(&request).is_err());
    }

    #[test]
    fn denied_rejects_injected_reservation_and_reserved_validates() {
        let denied: Result<CostReservationOutcomeV1, _> = serde_json::from_value(json!({
            "status": "denied",
            "reservation": serde_json::to_value(valid_reservation(&valid_request())).unwrap()
        }));
        assert!(denied.is_err());

        let request = valid_request();
        let outcome = CostReservationOutcomeV1::Reserved {
            reservation: valid_reservation(&request),
        };
        assert!(outcome.validate().is_ok());
        assert!(outcome.validate_against(&request).is_ok());
    }
}
