//! Telemetry for the registry's runs and calls.
use op_registry::cli::{Outcome, RunHooks, UsageFailure};
use op_registry::{CallHook, CallInfo};

/// Run-level hooks: usage errors are recorded, telemetry is flushed before the
/// runner writes its result.
pub struct GaitHooks;

impl RunHooks for GaitHooks {
    fn on_usage_error(&self, failure: &UsageFailure) {
        #[cfg(feature = "telemetry")]
        {
            let error = gait_cli::cli::error::CliError::usage(failure.message.clone());
            concord_telemetry::error_anyhow!(&error, source = "gait.cli");
        }
        #[cfg(not(feature = "telemetry"))]
        let _ = failure;
    }

    fn post_run(&self, outcome: &Outcome) {
        let _ = outcome;
        #[cfg(feature = "telemetry")]
        gait_core::telemetry::flush();
    }
}

/// Call-level hook: each op runs inside a `gait.cli` span named for the op,
/// parented on `TRACEPARENT`/`BAGGAGE`, and a failure is recorded once.
pub struct GaitCallHook;

impl CallHook for GaitCallHook {
    #[cfg(feature = "telemetry")]
    fn around<'a>(
        &'a self,
        info: &CallInfo,
        call: futures_util::future::BoxFuture<'a, ()>,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        use tracing::Instrument;
        let span = tracing::info_span!("gait.cli", op = %info.name);
        concord_telemetry::propagation::set_parent_from_env(&span);
        Box::pin(call.instrument(span))
    }

    fn on_error(&self, info: &CallInfo, err: &anyhow::Error) {
        let _ = info;
        #[cfg(feature = "telemetry")]
        if !gait_cli::cli::error::is_recorded(err) {
            concord_telemetry::error_anyhow!(err, source = "gait.cli");
        }
        #[cfg(not(feature = "telemetry"))]
        let _ = err;
    }
}
