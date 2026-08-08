use crate::kernel_handle::KernelHandle;
use crate::result_receipt::{canonical_json_sha256, receipt_payload};
use futures::FutureExt;
use openfang_types::runtime_admission::{
    ExecutionReceiptStatus, ExecutionReceiptV1, ReceiptFinalStatus, ReceiptFinalizationV1,
    ResultStorageMode, RuntimeAdmissionContextV1, RuntimeAdmissionDecisionV1,
};
use openfang_types::tool::ToolResult;
use openfang_types::tool_compat::normalize_tool_name;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

/// Marks the caller path for admission-controlled execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOrigin {
    AgentLoop,
    ExternalMcp(RuntimeAdmissionContextV1),
}

/// The closed outcomes exposed by admission-controlled external tool execution.
#[derive(Debug, Clone)]
pub enum RuntimeToolExecution {
    Completed(ToolResult),
    Replay {
        invocation_id: String,
        status: ExecutionReceiptStatus,
        result_sha256: Option<String>,
        result_storage_mode: Option<ResultStorageMode>,
        redacted_envelope: Option<String>,
    },
    Pending {
        reason_code: String,
    },
    Denied {
        reason_code: String,
    },
    CostUnavailable {
        reason_code: String,
    },
    InProgress {
        reason_code: String,
    },
    OutcomeUnknown {
        reason_code: String,
    },
}

/// Dispatch an external MCP tool only after durable admission and dispatch marking.
pub async fn execute_external_mcp_tool<F, Fut>(
    kernel: Arc<dyn KernelHandle>,
    context: RuntimeAdmissionContextV1,
    tool_use_id: &str,
    tool_name: &str,
    input: &serde_json::Value,
    caller_agent_id: Option<&str>,
    allowed_tools: Option<&[String]>,
    invoker: F,
) -> RuntimeToolExecution
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ToolResult>,
{
    let binding_denial: Option<&'static str> = if tool_use_id != context.invocation_id {
        Some("invocation_id_mismatch")
    } else if tool_name != context.tool_name {
        Some("tool_name_mismatch")
    } else if caller_agent_id != Some(context.caller_agent_id.as_str()) {
        Some("caller_agent_id_mismatch")
    } else if !allowed_tools.is_some_and(|allowed| {
        let normalized = normalize_tool_name(tool_name);
        allowed.iter().any(|candidate| candidate == normalized)
    }) {
        Some("tool_scope_denied")
    } else if match canonical_json_sha256(input) {
        Ok(digest) => digest != context.arguments_sha256,
        Err(_) => true,
    } {
        Some("arguments_digest_mismatch")
    } else {
        None
    };
    if let Some(reason_code) = binding_denial {
        return RuntimeToolExecution::Denied {
            reason_code: reason_code.into(),
        };
    }

    let receipt_max_bytes = match kernel.runtime_receipt_max_bytes() {
        Ok(bound) if (1..=65_536).contains(&bound) => bound,
        _ => {
            return RuntimeToolExecution::Denied {
                reason_code: "runtime_receipt_bound_unavailable".into(),
            }
        }
    };
    let invocation_id = context.invocation_id.clone();
    let decision = match kernel.runtime_admit(context).await {
        Ok(decision) => decision,
        Err(_) => {
            return RuntimeToolExecution::Denied {
                reason_code: "runtime_admission_unavailable".into(),
            }
        }
    };
    let prepared_receipt = match decision {
        RuntimeAdmissionDecisionV1::Denied { reason_code } => {
            return RuntimeToolExecution::Denied { reason_code };
        }
        RuntimeAdmissionDecisionV1::PendingApproval { .. } => {
            return RuntimeToolExecution::Pending {
                reason_code: "approval_pending".into(),
            };
        }
        RuntimeAdmissionDecisionV1::CostUnavailable { reason_code } => {
            return RuntimeToolExecution::CostUnavailable { reason_code };
        }
        RuntimeAdmissionDecisionV1::InProgress { receipt }
            if receipt.status == ExecutionReceiptStatus::Prepared =>
        {
            receipt
        }
        RuntimeAdmissionDecisionV1::InProgress { .. } => {
            return RuntimeToolExecution::InProgress {
                reason_code: "invocation_in_progress".into(),
            };
        }
        RuntimeAdmissionDecisionV1::OutcomeUnknown { .. } => {
            return RuntimeToolExecution::OutcomeUnknown {
                reason_code: "outcome_unknown".into(),
            };
        }
        RuntimeAdmissionDecisionV1::Replay { receipt } => {
            return replay_evidence(receipt, receipt_max_bytes);
        }
        RuntimeAdmissionDecisionV1::Admitted { receipt } => receipt,
    };
    if prepared_receipt.invocation_id != invocation_id
        || prepared_receipt.tool_name != tool_name
        || prepared_receipt.arguments_sha256 != canonical_json_sha256(input).unwrap_or_default()
    {
        return RuntimeToolExecution::Denied {
            reason_code: "prepared_receipt_binding_mismatch".into(),
        };
    }

    match kernel.runtime_mark_dispatching(&invocation_id).await {
        Ok(receipt) if receipt.status == ExecutionReceiptStatus::Dispatching => {}
        Ok(receipt) => return resolve_mark_receipt(receipt, receipt_max_bytes),
        Err(_) => {
            return match kernel.runtime_read_receipt(&invocation_id).await {
                Ok(Some(receipt)) => resolve_mark_receipt(receipt, receipt_max_bytes),
                _ => RuntimeToolExecution::OutcomeUnknown {
                    reason_code: "dispatch_state_unreadable".into(),
                },
            };
        }
    }

    // Once Dispatching is durable, dropping this future leaves recovery to the
    // kernel's restart/stale-dispatch reconciliation; no local cancellation
    // result is claimed.
    let invocation = match std::panic::catch_unwind(AssertUnwindSafe(invoker)) {
        Ok(invocation) => invocation,
        Err(_) => {
            let _ = kernel
                .runtime_mark_outcome_unknown(&invocation_id, "invoker_panic".into())
                .await;
            return RuntimeToolExecution::OutcomeUnknown {
                reason_code: "invoker_panic".into(),
            };
        }
    };
    let result = match AssertUnwindSafe(invocation).catch_unwind().await {
        Ok(result) => result,
        Err(_) => {
            let _ = kernel
                .runtime_mark_outcome_unknown(&invocation_id, "invoker_panic".into())
                .await;
            return RuntimeToolExecution::OutcomeUnknown {
                reason_code: "invoker_panic".into(),
            };
        }
    };
    let finalization = if result.is_error {
        ReceiptFinalizationV1 {
            status: ReceiptFinalStatus::Failed,
            actual_cost_micro_usd: 0,
            result_sha256: None,
            result_storage_mode: None,
            result_envelope: None,
            error_class: Some("tool_error".into()),
        }
    } else {
        let payload = match receipt_payload(&result, receipt_max_bytes) {
            Ok(payload) => payload,
            Err(_) => {
                let _ = kernel
                    .runtime_mark_outcome_unknown(&invocation_id, "receipt_payload_invalid".into())
                    .await;
                return RuntimeToolExecution::OutcomeUnknown {
                    reason_code: "receipt_payload_invalid".into(),
                };
            }
        };
        ReceiptFinalizationV1 {
            status: ReceiptFinalStatus::Succeeded,
            actual_cost_micro_usd: 0,
            result_sha256: Some(payload.sha256),
            result_storage_mode: Some(payload.storage_mode),
            result_envelope: payload.envelope,
            error_class: None,
        }
    };
    match kernel.runtime_finalize(&invocation_id, finalization).await {
        Ok(_) => RuntimeToolExecution::Completed(result),
        Err(_) => {
            let _ = kernel
                .runtime_mark_outcome_unknown(&invocation_id, "receipt_write_failed".into())
                .await;
            match kernel.runtime_read_receipt(&invocation_id).await {
                Ok(Some(receipt))
                    if matches!(
                        receipt.status,
                        ExecutionReceiptStatus::Succeeded | ExecutionReceiptStatus::Failed
                    ) =>
                {
                    replay_evidence(receipt, receipt_max_bytes)
                }
                _ => RuntimeToolExecution::OutcomeUnknown {
                    reason_code: "receipt_write_failed".into(),
                },
            }
        }
    }
}

fn resolve_mark_receipt(
    receipt: ExecutionReceiptV1,
    receipt_max_bytes: usize,
) -> RuntimeToolExecution {
    match receipt.status {
        ExecutionReceiptStatus::Prepared => RuntimeToolExecution::Denied {
            reason_code: "dispatch_intent_not_durable".into(),
        },
        ExecutionReceiptStatus::Dispatching => RuntimeToolExecution::OutcomeUnknown {
            reason_code: "dispatch_state_ambiguous".into(),
        },
        ExecutionReceiptStatus::Succeeded | ExecutionReceiptStatus::Failed => {
            replay_evidence(receipt, receipt_max_bytes)
        }
        ExecutionReceiptStatus::OutcomeUnknown => RuntimeToolExecution::OutcomeUnknown {
            reason_code: "outcome_unknown".into(),
        },
    }
}

fn replay_evidence(receipt: ExecutionReceiptV1, receipt_max_bytes: usize) -> RuntimeToolExecution {
    if receipt.validate(receipt_max_bytes).is_err()
        || !matches!(
            receipt.status,
            ExecutionReceiptStatus::Succeeded | ExecutionReceiptStatus::Failed
        )
    {
        return RuntimeToolExecution::OutcomeUnknown {
            reason_code: "replay_receipt_invalid".into(),
        };
    }
    let redacted_envelope = match (
        receipt.status,
        receipt.result_storage_mode,
        receipt.result_envelope,
    ) {
        (
            ExecutionReceiptStatus::Succeeded,
            Some(ResultStorageMode::RedactedEnvelope),
            Some(envelope),
        ) if serde_json::from_str::<serde_json::Value>(&envelope).is_ok() => Some(envelope),
        (ExecutionReceiptStatus::Succeeded, Some(ResultStorageMode::DigestOnly), None)
        | (ExecutionReceiptStatus::Failed, None, None) => None,
        _ => {
            return RuntimeToolExecution::OutcomeUnknown {
                reason_code: "replay_receipt_invalid".into(),
            };
        }
    };
    RuntimeToolExecution::Replay {
        invocation_id: receipt.invocation_id,
        status: receipt.status,
        result_sha256: receipt.result_sha256,
        result_storage_mode: receipt.result_storage_mode,
        redacted_envelope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_handle::{AgentInfo, KernelHandle};
    use crate::result_receipt::canonical_json_sha256;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use openfang_types::runtime_admission::{
        ExecutionReceiptStatus, ExecutionReceiptV1, ResultStorageMode, RuntimeAdmissionContextV1,
        RuntimeAdmissionDecisionV1,
    };
    use openfang_types::tool::ToolResult;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct FakeKernel {
        decision: Mutex<RuntimeAdmissionDecisionV1>,
        receipt: Mutex<Option<ExecutionReceiptV1>>,
        admission_calls: AtomicUsize,
        dispatches: AtomicUsize,
        unknowns: AtomicUsize,
        mark_results: Mutex<VecDeque<Result<ExecutionReceiptV1, String>>>,
        fail_finalize: bool,
        finalize_commits_before_error: bool,
        fail_unknown: bool,
        receipt_max_bytes: usize,
    }

    impl FakeKernel {
        fn admitted() -> Arc<Self> {
            Arc::new(Self {
                decision: Mutex::new(RuntimeAdmissionDecisionV1::Admitted {
                    receipt: receipt(ExecutionReceiptStatus::Prepared),
                }),
                receipt: Mutex::new(None),
                admission_calls: AtomicUsize::new(0),
                dispatches: AtomicUsize::new(0),
                unknowns: AtomicUsize::new(0),
                mark_results: Mutex::new(VecDeque::new()),
                fail_finalize: false,
                finalize_commits_before_error: false,
                fail_unknown: false,
                receipt_max_bytes: 1_024,
            })
        }
    }

    #[async_trait]
    impl KernelHandle for FakeKernel {
        async fn spawn_agent(&self, _: &str, _: Option<&str>) -> Result<(String, String), String> {
            Err("unused".into())
        }
        async fn send_to_agent(&self, _: &str, _: &str) -> Result<String, String> {
            Err("unused".into())
        }
        async fn runtime_admit(
            &self,
            _: RuntimeAdmissionContextV1,
        ) -> Result<RuntimeAdmissionDecisionV1, String> {
            self.admission_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.decision.lock().expect("lock").clone())
        }
        async fn runtime_mark_dispatching(&self, _: &str) -> Result<ExecutionReceiptV1, String> {
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            if let Some(outcome) = self.mark_results.lock().expect("lock").pop_front() {
                if let Ok(receipt) = &outcome {
                    *self.receipt.lock().expect("lock") = Some(receipt.clone());
                }
                return outcome;
            }
            let dispatching = receipt(ExecutionReceiptStatus::Dispatching);
            *self.receipt.lock().expect("lock") = Some(dispatching.clone());
            Ok(dispatching)
        }
        async fn runtime_finalize(
            &self,
            _: &str,
            update: openfang_types::runtime_admission::ReceiptFinalizationV1,
        ) -> Result<ExecutionReceiptV1, String> {
            let mut final_receipt = receipt(match update.status {
                openfang_types::runtime_admission::ReceiptFinalStatus::Succeeded => {
                    ExecutionReceiptStatus::Succeeded
                }
                openfang_types::runtime_admission::ReceiptFinalStatus::Failed => {
                    ExecutionReceiptStatus::Failed
                }
                openfang_types::runtime_admission::ReceiptFinalStatus::OutcomeUnknown => {
                    ExecutionReceiptStatus::OutcomeUnknown
                }
            });
            final_receipt.result_sha256 = update.result_sha256;
            final_receipt.result_storage_mode = update.result_storage_mode;
            final_receipt.result_envelope = update.result_envelope;
            final_receipt.error_class = update.error_class;
            if !self.fail_finalize || self.finalize_commits_before_error {
                *self.receipt.lock().expect("lock") = Some(final_receipt.clone());
            }
            if self.fail_finalize {
                return Err("persistence details must not escape".into());
            }
            Ok(final_receipt)
        }
        async fn runtime_mark_outcome_unknown(
            &self,
            _: &str,
            _: String,
        ) -> Result<ExecutionReceiptV1, String> {
            self.unknowns.fetch_add(1, Ordering::SeqCst);
            if self.fail_unknown {
                return Err("still unavailable".into());
            }
            if self
                .receipt
                .lock()
                .expect("lock")
                .as_ref()
                .is_some_and(|receipt| {
                    matches!(
                        receipt.status,
                        ExecutionReceiptStatus::Succeeded | ExecutionReceiptStatus::Failed
                    )
                })
            {
                return Err("terminal receipt cannot become unknown".into());
            }
            let unknown = receipt(ExecutionReceiptStatus::OutcomeUnknown);
            *self.receipt.lock().expect("lock") = Some(unknown.clone());
            Ok(unknown)
        }
        async fn runtime_read_receipt(
            &self,
            _: &str,
        ) -> Result<Option<ExecutionReceiptV1>, String> {
            Ok(self.receipt.lock().expect("lock").clone())
        }
        fn runtime_receipt_max_bytes(&self) -> Result<usize, String> {
            Ok(self.receipt_max_bytes)
        }
        fn list_agents(&self) -> Vec<AgentInfo> {
            vec![]
        }
        fn kill_agent(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn memory_store(&self, _: &str, _: serde_json::Value) -> Result<(), String> {
            Ok(())
        }
        fn memory_recall(&self, _: &str) -> Result<Option<serde_json::Value>, String> {
            Ok(None)
        }
        fn find_agents(&self, _: &str) -> Vec<AgentInfo> {
            vec![]
        }
        async fn task_post(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String, String> {
            Err("unused".into())
        }
        async fn task_claim(&self, _: &str) -> Result<Option<serde_json::Value>, String> {
            Ok(None)
        }
        async fn task_complete(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn task_list(&self, _: Option<&str>) -> Result<Vec<serde_json::Value>, String> {
            Ok(vec![])
        }
        async fn publish_event(&self, _: &str, _: serde_json::Value) -> Result<(), String> {
            Ok(())
        }
        async fn knowledge_add_entity(
            &self,
            _: openfang_types::memory::Entity,
        ) -> Result<String, String> {
            Err("unused".into())
        }
        async fn knowledge_add_relation(
            &self,
            _: openfang_types::memory::Relation,
        ) -> Result<String, String> {
            Err("unused".into())
        }
        async fn knowledge_query(
            &self,
            _: openfang_types::memory::GraphPattern,
        ) -> Result<Vec<openfang_types::memory::GraphMatch>, String> {
            Ok(vec![])
        }
    }

    fn receipt(status: ExecutionReceiptStatus) -> ExecutionReceiptV1 {
        let now = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("time");
        ExecutionReceiptV1 {
            invocation_id: "invocation-1".into(),
            correlation_id: "corr-1".into(),
            plan_id: "plan-1".into(),
            plan_revision: 1,
            space_id: "space-1".into(),
            agent_id: "agent-1".into(),
            tool_name: "external.echo".into(),
            arguments_sha256: canonical_json_sha256(&serde_json::json!({"ok": true}))
                .expect("digest"),
            approval_ref: "approval-1".into(),
            cost_ref: "cost-1".into(),
            status,
            result_sha256: None,
            result_storage_mode: None,
            result_envelope: None,
            error_class: (matches!(
                status,
                ExecutionReceiptStatus::Failed | ExecutionReceiptStatus::OutcomeUnknown
            ))
            .then(|| "tool_error".into()),
            prepared_at: now,
            dispatch_started_at: (!matches!(status, ExecutionReceiptStatus::Prepared))
                .then_some(now),
            finished_at: (matches!(
                status,
                ExecutionReceiptStatus::Succeeded
                    | ExecutionReceiptStatus::Failed
                    | ExecutionReceiptStatus::OutcomeUnknown
            ))
            .then_some(now),
        }
    }

    fn context() -> RuntimeAdmissionContextV1 {
        RuntimeAdmissionContextV1 {
            invocation_id: "invocation-1".into(),
            caller_agent_id: "agent-1".into(),
            approval_ref: "approval-1".into(),
            cost_ref: "cost-1".into(),
            tool_name: "external.echo".into(),
            arguments_sha256: canonical_json_sha256(&serde_json::json!({"ok": true}))
                .expect("digest"),
        }
    }

    async fn execute_for_test<F, Fut>(
        kernel: Arc<dyn KernelHandle>,
        context: RuntimeAdmissionContextV1,
        invoker: F,
    ) -> RuntimeToolExecution
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ToolResult>,
    {
        let input = serde_json::json!({"ok": true});
        let invocation_id = context.invocation_id.clone();
        let tool_name = context.tool_name.clone();
        let caller_agent_id = context.caller_agent_id.clone();
        let allowed_tools = vec![tool_name.clone()];
        execute_external_mcp_tool(
            kernel,
            context,
            &invocation_id,
            &tool_name,
            &input,
            Some(&caller_agent_id),
            Some(&allowed_tools),
            invoker,
        )
        .await
    }

    #[tokio::test]
    async fn binding_mismatches_stop_before_admission_or_dispatch() {
        enum Mismatch {
            Invocation,
            Tool,
            Caller,
            Digest,
            Scope,
        }
        for mismatch in [
            Mismatch::Invocation,
            Mismatch::Tool,
            Mismatch::Caller,
            Mismatch::Digest,
            Mismatch::Scope,
        ] {
            let kernel = FakeKernel::admitted();
            let mut context = context();
            let mut tool_use_id = context.invocation_id.clone();
            let mut tool_name = context.tool_name.clone();
            let mut caller = Some(context.caller_agent_id.clone());
            let input = serde_json::json!({"ok": true});
            let mut allowed = Some(vec![context.tool_name.clone()]);
            match mismatch {
                Mismatch::Invocation => tool_use_id = "wrong-invocation".into(),
                Mismatch::Tool => tool_name = "wrong.tool".into(),
                Mismatch::Caller => caller = Some("wrong-agent".into()),
                Mismatch::Digest => context.arguments_sha256 = "b".repeat(64),
                Mismatch::Scope => allowed = Some(vec!["other.tool".into()]),
            }
            let invocations = AtomicUsize::new(0);
            let outcome = execute_external_mcp_tool(
                kernel.clone(),
                context,
                &tool_use_id,
                &tool_name,
                &input,
                caller.as_deref(),
                allowed.as_deref(),
                || async {
                    invocations.fetch_add(1, Ordering::SeqCst);
                    panic!("binding mismatch must not invoke")
                },
            )
            .await;
            assert!(matches!(outcome, RuntimeToolExecution::Denied { .. }));
            assert_eq!(kernel.admission_calls.load(Ordering::SeqCst), 0);
            assert_eq!(kernel.dispatches.load(Ordering::SeqCst), 0);
            assert_eq!(invocations.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn external_denied_or_pending_never_calls_invoker() {
        for decision in [
            RuntimeAdmissionDecisionV1::Denied {
                reason_code: "policy_denied".into(),
            },
            RuntimeAdmissionDecisionV1::PendingApproval {
                approval_ref: "approval-1".into(),
            },
        ] {
            let kernel = FakeKernel::admitted();
            *kernel.decision.lock().expect("lock") = decision;
            let calls = AtomicUsize::new(0);
            let outcome = execute_for_test(kernel, context(), || async {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("invoker must not run")
            })
            .await;
            assert!(matches!(
                outcome,
                RuntimeToolExecution::Denied { .. } | RuntimeToolExecution::Pending { .. }
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn admitted_invocation_dispatches_once_and_persists_bound_receipt() {
        let kernel = FakeKernel::admitted();
        let outcome = execute_for_test(kernel.clone(), context(), || async {
            ToolResult {
                tool_use_id: "ignored".into(),
                content: r#"{"ok":true}"#.into(),
                is_error: false,
            }
        })
        .await;
        assert!(
            matches!(outcome, RuntimeToolExecution::Completed(result) if result.content == r#"{"ok":true}"#)
        );
        assert_eq!(kernel.dispatches.load(Ordering::SeqCst), 1);
        let receipt = kernel
            .receipt
            .lock()
            .expect("lock")
            .clone()
            .expect("final receipt");
        assert_eq!(receipt.status, ExecutionReceiptStatus::Succeeded);
    }

    #[tokio::test]
    async fn in_progress_prepared_can_claim_while_dispatching_never_invokes() {
        let prepared_kernel = FakeKernel::admitted();
        *prepared_kernel.decision.lock().expect("lock") = RuntimeAdmissionDecisionV1::InProgress {
            receipt: receipt(ExecutionReceiptStatus::Prepared),
        };
        let prepared_calls = AtomicUsize::new(0);
        let prepared_outcome = execute_for_test(prepared_kernel.clone(), context(), || async {
            prepared_calls.fetch_add(1, Ordering::SeqCst);
            ToolResult {
                tool_use_id: "invocation-1".into(),
                content: r#"{"ok":true}"#.into(),
                is_error: false,
            }
        })
        .await;
        assert!(matches!(
            prepared_outcome,
            RuntimeToolExecution::Completed(_)
        ));
        assert_eq!(prepared_calls.load(Ordering::SeqCst), 1);

        let dispatching_kernel = FakeKernel::admitted();
        *dispatching_kernel.decision.lock().expect("lock") =
            RuntimeAdmissionDecisionV1::InProgress {
                receipt: receipt(ExecutionReceiptStatus::Dispatching),
            };
        let dispatching_calls = AtomicUsize::new(0);
        let dispatching_outcome =
            execute_for_test(dispatching_kernel.clone(), context(), || async {
                dispatching_calls.fetch_add(1, Ordering::SeqCst);
                panic!("dispatching receipt must not invoke")
            })
            .await;
        assert!(matches!(
            dispatching_outcome,
            RuntimeToolExecution::InProgress { .. }
        ));
        assert_eq!(dispatching_calls.load(Ordering::SeqCst), 0);
        assert_eq!(dispatching_kernel.dispatches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn kernel_receipt_bound_controls_retention_without_caller_override() {
        let kernel = Arc::new(FakeKernel {
            receipt_max_bytes: 4,
            ..FakeKernel {
                decision: Mutex::new(RuntimeAdmissionDecisionV1::Admitted {
                    receipt: receipt(ExecutionReceiptStatus::Prepared),
                }),
                receipt: Mutex::new(None),
                admission_calls: AtomicUsize::new(0),
                dispatches: AtomicUsize::new(0),
                unknowns: AtomicUsize::new(0),
                mark_results: Mutex::new(VecDeque::new()),
                fail_finalize: false,
                finalize_commits_before_error: false,
                fail_unknown: false,
                receipt_max_bytes: 1_024,
            }
        });

        let outcome = execute_for_test(kernel.clone(), context(), || async {
            ToolResult {
                tool_use_id: "invocation-1".into(),
                content: r#"{"retained":false}"#.into(),
                is_error: false,
            }
        })
        .await;

        assert!(matches!(outcome, RuntimeToolExecution::Completed(_)));
        assert_eq!(
            kernel
                .receipt
                .lock()
                .expect("lock")
                .as_ref()
                .and_then(|receipt| receipt.result_storage_mode),
            Some(ResultStorageMode::DigestOnly)
        );
    }

    #[tokio::test]
    async fn prepared_mark_failure_can_retry_and_dispatch_once() {
        let kernel = FakeKernel::admitted();
        *kernel.receipt.lock().expect("lock") = Some(receipt(ExecutionReceiptStatus::Prepared));
        kernel.mark_results.lock().expect("lock").extend([
            Err("write failed before commit".into()),
            Ok(receipt(ExecutionReceiptStatus::Dispatching)),
        ]);
        let invocations = AtomicUsize::new(0);

        let first = execute_for_test(kernel.clone(), context(), || async {
            invocations.fetch_add(1, Ordering::SeqCst);
            panic!("precommit failure must not invoke")
        })
        .await;
        assert!(matches!(
            first,
            RuntimeToolExecution::Denied { ref reason_code }
                if reason_code == "dispatch_intent_not_durable"
        ));

        let second = execute_for_test(kernel.clone(), context(), || async {
            invocations.fetch_add(1, Ordering::SeqCst);
            ToolResult {
                tool_use_id: "invocation-1".into(),
                content: r#"{"ok":true}"#.into(),
                is_error: false,
            }
        })
        .await;
        assert!(matches!(second, RuntimeToolExecution::Completed(_)));
        assert_eq!(invocations.load(Ordering::SeqCst), 1);
        assert_eq!(kernel.dispatches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn postcommit_mark_failure_never_invokes() {
        let kernel = FakeKernel::admitted();
        *kernel.receipt.lock().expect("lock") = Some(receipt(ExecutionReceiptStatus::Dispatching));
        kernel
            .mark_results
            .lock()
            .expect("lock")
            .push_back(Err("ambiguous write".into()));
        let invocations = AtomicUsize::new(0);

        let outcome = execute_for_test(kernel.clone(), context(), || async {
            invocations.fetch_add(1, Ordering::SeqCst);
            panic!("ambiguous dispatch mark must not invoke")
        })
        .await;

        assert!(matches!(
            outcome,
            RuntimeToolExecution::OutcomeUnknown { .. }
        ));
        assert_eq!(invocations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn invoker_panic_marks_durable_unknown() {
        let kernel = FakeKernel::admitted();

        let outcome = execute_for_test(kernel.clone(), context(), || async {
            panic!("synthetic invoker panic")
        })
        .await;

        assert!(matches!(
            outcome,
            RuntimeToolExecution::OutcomeUnknown { .. }
        ));
        assert_eq!(kernel.unknowns.load(Ordering::SeqCst), 1);
        assert_eq!(
            kernel
                .receipt
                .lock()
                .expect("lock")
                .as_ref()
                .map(|receipt| receipt.status),
            Some(ExecutionReceiptStatus::OutcomeUnknown)
        );
    }

    #[tokio::test]
    async fn finalize_error_resolves_committed_receipt_as_replay() {
        let kernel = Arc::new(FakeKernel {
            fail_finalize: true,
            finalize_commits_before_error: true,
            ..FakeKernel {
                decision: Mutex::new(RuntimeAdmissionDecisionV1::Admitted {
                    receipt: receipt(ExecutionReceiptStatus::Prepared),
                }),
                receipt: Mutex::new(None),
                admission_calls: AtomicUsize::new(0),
                dispatches: AtomicUsize::new(0),
                unknowns: AtomicUsize::new(0),
                mark_results: Mutex::new(VecDeque::new()),
                fail_finalize: false,
                finalize_commits_before_error: false,
                fail_unknown: false,
                receipt_max_bytes: 1_024,
            }
        });

        let outcome = execute_for_test(kernel.clone(), context(), || async {
            ToolResult {
                tool_use_id: "invocation-1".into(),
                content: r#"{"ok":true}"#.into(),
                is_error: false,
            }
        })
        .await;

        assert!(matches!(
            outcome,
            RuntimeToolExecution::Replay {
                status: ExecutionReceiptStatus::Succeeded,
                ..
            }
        ));
        assert_eq!(kernel.unknowns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn finalize_and_unknown_write_failures_return_outcome_unknown() {
        for fail_unknown in [false, true] {
            let kernel = Arc::new(FakeKernel {
                fail_finalize: true,
                fail_unknown,
                ..FakeKernel {
                    decision: Mutex::new(RuntimeAdmissionDecisionV1::Admitted {
                        receipt: receipt(ExecutionReceiptStatus::Prepared),
                    }),
                    receipt: Mutex::new(None),
                    admission_calls: AtomicUsize::new(0),
                    dispatches: AtomicUsize::new(0),
                    unknowns: AtomicUsize::new(0),
                    mark_results: Mutex::new(VecDeque::new()),
                    fail_finalize: false,
                    finalize_commits_before_error: false,
                    fail_unknown: false,
                    receipt_max_bytes: 1_024,
                }
            });
            let calls = AtomicUsize::new(0);
            let attempt_context = context();
            let outcome = execute_for_test(kernel.clone(), attempt_context.clone(), || async {
                calls.fetch_add(1, Ordering::SeqCst);
                ToolResult {
                    tool_use_id: "x".into(),
                    content: r#"{"ok":true}"#.into(),
                    is_error: false,
                }
            })
            .await;
            assert!(matches!(
                outcome,
                RuntimeToolExecution::OutcomeUnknown { .. }
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(kernel.unknowns.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn replay_never_invokes_and_returns_receipt_bound_evidence() {
        for replay_receipt in [
            {
                let mut r = receipt(ExecutionReceiptStatus::Succeeded);
                r.result_sha256 = Some("b".repeat(64));
                r.result_storage_mode = Some(ResultStorageMode::RedactedEnvelope);
                r.result_envelope = Some(r#"{"ok":true}"#.into());
                r
            },
            receipt(ExecutionReceiptStatus::Failed),
            {
                let mut r = receipt(ExecutionReceiptStatus::Succeeded);
                r.result_sha256 = Some("b".repeat(64));
                r.result_storage_mode = Some(ResultStorageMode::DigestOnly);
                r
            },
        ] {
            let kernel = FakeKernel::admitted();
            let expected = replay_receipt.clone();
            *kernel.decision.lock().expect("lock") = RuntimeAdmissionDecisionV1::Replay {
                receipt: replay_receipt,
            };
            let calls = AtomicUsize::new(0);
            let outcome = execute_for_test(kernel, context(), || async {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("replay must not invoke")
            })
            .await;
            let RuntimeToolExecution::Replay {
                invocation_id,
                status,
                result_sha256,
                result_storage_mode,
                redacted_envelope,
            } = outcome
            else {
                panic!("expected receipt-bound replay evidence")
            };
            assert_eq!(invocation_id, expected.invocation_id);
            assert_eq!(status, expected.status);
            assert_eq!(result_sha256, expected.result_sha256);
            assert_eq!(result_storage_mode, expected.result_storage_mode);
            assert_eq!(redacted_envelope, expected.result_envelope);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
}
