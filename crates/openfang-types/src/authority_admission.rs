//! Fail-closed transport types for OpenFang authority admission.

use serde::{Deserialize, Deserializer, Serialize};

/// The schema version accepted by this authority-admission contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityAdmissionContractVersion {
    V1,
}

/// A requested role and the positive number of agents requested for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityAdmissionRole {
    pub role: String,
    pub count: u32,
}

/// A versioned request for OpenFang to admit an already-planned operation.
///
/// This is correlation metadata only. It does not carry an approval or cost
/// decision, and `handoff_proof_ref` remains opaque to this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityAdmissionRequestV1 {
    pub contract_version: AuthorityAdmissionContractVersion,
    pub correlation_id: String,
    pub plan_id: String,
    pub plan_revision: u64,
    pub space_id: String,
    pub agent_id: String,
    pub roles: Vec<AuthorityAdmissionRole>,
    pub handoff_proof_ref: String,
}

impl AuthorityAdmissionRequestV1 {
    /// Validate the structural identity required for an admission request.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("correlation_id", &self.correlation_id),
            ("plan_id", &self.plan_id),
            ("space_id", &self.space_id),
            ("agent_id", &self.agent_id),
            ("handoff_proof_ref", &self.handoff_proof_ref),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} must not be empty"));
            }
        }

        if self.plan_revision == 0 {
            return Err("plan_revision must be positive".into());
        }

        if self.roles.is_empty() {
            return Err("roles must not be empty".into());
        }

        for (index, role) in self.roles.iter().enumerate() {
            if role.role.trim().is_empty() {
                return Err(format!("roles[{index}].role must not be empty"));
            }
            if role.count == 0 {
                return Err(format!("roles[{index}].count must be positive"));
            }
        }

        Ok(())
    }
}

/// The only outcomes OpenFang may report for an admission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthorityAdmissionOutcomeV1 {
    Denied,
    PendingApproval {
        approval_ref: String,
    },
    Admitted {
        approval_ref: String,
        cost_ref: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum AuthorityAdmissionOutcomeV1Wire {
    Denied {},
    PendingApproval {
        approval_ref: String,
    },
    Admitted {
        approval_ref: String,
        cost_ref: String,
    },
}

impl<'de> Deserialize<'de> for AuthorityAdmissionOutcomeV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match AuthorityAdmissionOutcomeV1Wire::deserialize(deserializer)? {
            AuthorityAdmissionOutcomeV1Wire::Denied {} => Ok(Self::Denied),
            AuthorityAdmissionOutcomeV1Wire::PendingApproval { approval_ref } => {
                Ok(Self::PendingApproval { approval_ref })
            }
            AuthorityAdmissionOutcomeV1Wire::Admitted {
                approval_ref,
                cost_ref,
            } => Ok(Self::Admitted {
                approval_ref,
                cost_ref,
            }),
        }
    }
}

impl AuthorityAdmissionOutcomeV1 {
    /// Validate references required by each closed admission outcome.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Denied => Ok(()),
            Self::PendingApproval { approval_ref } => {
                if approval_ref.trim().is_empty() {
                    Err("approval_ref must not be empty for pending_approval".into())
                } else {
                    Ok(())
                }
            }
            Self::Admitted {
                approval_ref,
                cost_ref,
            } => {
                if approval_ref.trim().is_empty() {
                    return Err("approval_ref must not be empty for admitted".into());
                }
                if cost_ref.trim().is_empty() {
                    return Err("cost_ref must not be empty for admitted".into());
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_request() -> AuthorityAdmissionRequestV1 {
        AuthorityAdmissionRequestV1 {
            contract_version: AuthorityAdmissionContractVersion::V1,
            correlation_id: "corr-001".into(),
            plan_id: "plan-001".into(),
            plan_revision: 1,
            space_id: "coding".into(),
            agent_id: "coding-agent".into(),
            roles: vec![AuthorityAdmissionRole {
                role: "implementer".into(),
                count: 1,
            }],
            handoff_proof_ref: "proof:already-planned-operation".into(),
        }
    }

    #[test]
    fn request_round_trips_and_validates() {
        let request = valid_request();
        request.validate().unwrap();

        let json = serde_json::to_string(&request).unwrap();
        let restored: AuthorityAdmissionRequestV1 = serde_json::from_str(&json).unwrap();

        assert_eq!(restored, request);
    }

    #[test]
    fn request_rejects_client_supplied_approval_ref() {
        let result: Result<AuthorityAdmissionRequestV1, _> = serde_json::from_value(json!({
            "contract_version": "v1",
            "correlation_id": "corr-001",
            "plan_id": "plan-001",
            "plan_revision": 1,
            "space_id": "coding",
            "agent_id": "coding-agent",
            "roles": [{"role": "implementer", "count": 1}],
            "handoff_proof_ref": "proof:already-planned-operation",
            "approval_ref": "client-must-not-supply-this"
        }));

        assert!(result.is_err(), "client approval_ref must be rejected");
    }

    #[test]
    fn request_rejects_client_supplied_cost_ref() {
        let result: Result<AuthorityAdmissionRequestV1, _> = serde_json::from_value(json!({
            "contract_version": "v1",
            "correlation_id": "corr-001",
            "plan_id": "plan-001",
            "plan_revision": 1,
            "space_id": "coding",
            "agent_id": "coding-agent",
            "roles": [{"role": "implementer", "count": 1}],
            "handoff_proof_ref": "proof:already-planned-operation",
            "cost_ref": "client-must-not-supply-this"
        }));

        assert!(result.is_err(), "client cost_ref must be rejected");
    }

    #[test]
    fn request_validation_rejects_empty_identity_and_invalid_roles() {
        let mut request = valid_request();
        request.correlation_id.clear();
        assert!(request.validate().unwrap_err().contains("correlation_id"));

        let mut request = valid_request();
        request.roles.clear();
        assert!(request.validate().unwrap_err().contains("roles"));

        let mut request = valid_request();
        request.roles[0].count = 0;
        assert!(request.validate().unwrap_err().contains("count"));
    }

    #[test]
    fn request_validation_rejects_every_empty_identity_string() {
        let mut request = valid_request();
        request.plan_id.clear();
        assert!(request.validate().unwrap_err().contains("plan_id"));

        let mut request = valid_request();
        request.space_id.clear();
        assert!(request.validate().unwrap_err().contains("space_id"));

        let mut request = valid_request();
        request.agent_id.clear();
        assert!(request.validate().unwrap_err().contains("agent_id"));

        let mut request = valid_request();
        request.handoff_proof_ref.clear();
        assert!(request
            .validate()
            .unwrap_err()
            .contains("handoff_proof_ref"));

        let mut request = valid_request();
        request.roles[0].role.clear();
        assert!(request.validate().unwrap_err().contains("role"));
    }

    #[test]
    fn request_validation_rejects_whitespace_only_identity_and_zero_revision() {
        let mut request = valid_request();
        request.space_id = " \t ".into();
        assert!(request.validate().unwrap_err().contains("space_id"));

        let mut request = valid_request();
        request.handoff_proof_ref = "\n".into();
        assert!(request
            .validate()
            .unwrap_err()
            .contains("handoff_proof_ref"));

        let mut request = valid_request();
        request.roles[0].role = " \t ".into();
        assert!(request.validate().unwrap_err().contains("role"));

        let mut request = valid_request();
        request.plan_revision = 0;
        assert!(request.validate().unwrap_err().contains("plan_revision"));
    }

    #[test]
    fn pending_approval_requires_nonempty_approval_ref_without_cost_ref() {
        let valid = AuthorityAdmissionOutcomeV1::PendingApproval {
            approval_ref: "approval:pending-001".into(),
        };
        assert!(valid.validate().is_ok());

        let missing_ref: AuthorityAdmissionOutcomeV1 = serde_json::from_value(json!({
            "status": "pending_approval",
            "approval_ref": ""
        }))
        .unwrap();
        assert!(missing_ref.validate().unwrap_err().contains("approval_ref"));

        let with_cost: Result<AuthorityAdmissionOutcomeV1, _> = serde_json::from_value(json!({
            "status": "pending_approval",
            "approval_ref": "approval:pending-001",
            "cost_ref": "cost:must-not-be-pending"
        }));
        assert!(with_cost.is_err(), "pending approval must reject cost_ref");
    }

    #[test]
    fn admitted_requires_nonempty_approval_and_cost_refs() {
        let valid = AuthorityAdmissionOutcomeV1::Admitted {
            approval_ref: "approval:approved-001".into(),
            cost_ref: "cost:reserved-001".into(),
        };
        assert!(valid.validate().is_ok());

        let missing_ref: AuthorityAdmissionOutcomeV1 = serde_json::from_value(json!({
            "status": "admitted",
            "approval_ref": "approval:approved-001",
            "cost_ref": ""
        }))
        .unwrap();
        assert!(missing_ref.validate().unwrap_err().contains("cost_ref"));
    }

    #[test]
    fn outcome_validation_rejects_whitespace_only_authority_and_cost_refs() {
        let pending = AuthorityAdmissionOutcomeV1::PendingApproval {
            approval_ref: "\t".into(),
        };
        assert!(pending.validate().unwrap_err().contains("approval_ref"));

        let admitted = AuthorityAdmissionOutcomeV1::Admitted {
            approval_ref: "approval:approved-001".into(),
            cost_ref: " \n ".into(),
        };
        assert!(admitted.validate().unwrap_err().contains("cost_ref"));
    }

    #[test]
    fn denied_carries_no_authority_or_cost_refs() {
        let denied = AuthorityAdmissionOutcomeV1::Denied;
        assert!(denied.validate().is_ok());

        let with_refs: Result<AuthorityAdmissionOutcomeV1, _> = serde_json::from_value(json!({
            "status": "denied",
            "approval_ref": "approval:must-not-be-denied",
            "cost_ref": "cost:must-not-be-denied"
        }));
        assert!(
            with_refs.is_err(),
            "denied must reject authority and cost refs"
        );
    }

    #[test]
    fn outcome_rejects_unknown_status_without_a_fallback() {
        let result: Result<AuthorityAdmissionOutcomeV1, _> = serde_json::from_value(json!({
            "status": "approved"
        }));

        assert!(
            result.is_err(),
            "only the closed outcome states are accepted"
        );
    }
}
