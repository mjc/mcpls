//! Shared request instrumentation for the MCP server boundary.
#![expect(deprecated)]
#![allow(clippy::ignored_unit_patterns)]

use std::{
    borrow::Cow,
    collections::HashMap,
    future::Future,
    io::{self, Write},
    time::Instant,
};

use opentelemetry::propagation::{TextMapCompositePropagator, TextMapPropagator};
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use rmcp::{
    ErrorData, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CompleteRequestParams, CustomRequest,
        CustomResult, DiscoverResult, GetPromptRequestParams, GetPromptResponse, GetTaskParams,
        GetTaskResult, InitializeRequestParams, InitializeResult, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResponse, ServerInfo, SetLevelRequestParams,
        SubscribeRequestParams, SubscriptionFilter, UnsubscribeRequestParams,
    },
    service::{NotificationContext, RequestContext, RoleServer, SubscriptionContext},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const MAX_TRACEPARENT_BYTES: usize = 55;
const MAX_TRACESTATE_BYTES: usize = 512;
const MAX_BAGGAGE_BYTES: usize = 8 * 1024;

#[derive(Default)]
struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn completed_tool_result_bytes(result: &Result<CallToolResponse, ErrorData>) -> Option<usize> {
    let Ok(CallToolResponse::Complete(result)) = result else {
        return None;
    };
    let mut bytes = ByteCounter::default();
    serde_json::to_writer(&mut bytes, result)
        .ok()
        .map(|()| bytes.0)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ToolResultMetrics {
    cache_hit: Option<bool>,
    item_count: usize,
    deferred_bytes: usize,
    truncated: bool,
    paginated: bool,
}

fn completed_tool_result_metrics(
    result: &Result<CallToolResponse, ErrorData>,
) -> Option<ToolResultMetrics> {
    let Ok(CallToolResponse::Complete(result)) = result else {
        return None;
    };
    let value = result.structured_content.as_ref()?;
    let object = value.as_object()?;
    let item_count = [
        "returned",
        "returned_items",
        "returned_lines",
        "returned_diagnostics",
        "returned_references",
        "returned_calls",
        "returned_groups",
    ]
    .into_iter()
    .find_map(|name| object.get(name).and_then(serde_json::Value::as_u64))
    .and_then(|count| usize::try_from(count).ok())
    .or_else(|| {
        ["items", "matches", "diagnostics"]
            .into_iter()
            .find_map(|name| object.get(name).and_then(serde_json::Value::as_array))
            .map(Vec::len)
    })
    .unwrap_or_default();
    let mut metrics = ToolResultMetrics {
        cache_hit: object
            .get("cache_hit")
            .and_then(serde_json::Value::as_bool)
            .or_else(|| {
                object
                    .get("cache")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|cache| cache.get("hit"))
                    .and_then(serde_json::Value::as_bool)
            }),
        item_count,
        truncated: object
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        paginated: ["next_cursor", "nextCursor"]
            .into_iter()
            .any(|name| object.get(name).is_some_and(|value| !value.is_null())),
        ..ToolResultMetrics::default()
    };
    collect_deferred_bytes(value, &mut metrics.deferred_bytes);
    Some(metrics)
}

fn collect_deferred_bytes(value: &serde_json::Value, deferred_bytes: &mut usize) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_deferred_bytes(value, deferred_bytes);
            }
        }
        serde_json::Value::Object(object) => {
            if object.contains_key("uri")
                && let Some(bytes) = object
                    .get("total_bytes")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|bytes| usize::try_from(bytes).ok())
            {
                *deferred_bytes = deferred_bytes.saturating_add(bytes);
            }
            for value in object.values() {
                collect_deferred_bytes(value, deferred_bytes);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteTrace {
    trace_id: String,
    span_id: String,
}

fn parse_remote_trace(meta: &rmcp::model::RequestMetaObject) -> Option<RemoteTrace> {
    let traceparent = meta.get_traceparent()?;
    let valid_tracestate = meta
        .get_tracestate()
        .is_none_or(|value| value.len() <= MAX_TRACESTATE_BYTES && value.is_ascii());
    let valid_baggage = meta
        .get_baggage()
        .is_none_or(|value| value.len() <= MAX_BAGGAGE_BYTES && value.is_ascii());

    if traceparent.len() > MAX_TRACEPARENT_BYTES
        || !valid_tracestate
        || !valid_baggage
        || traceparent.len() != MAX_TRACEPARENT_BYTES
        || !traceparent.is_ascii()
    {
        tracing::warn!("ignoring malformed or oversized MCP trace context");
        return None;
    }

    let bytes = traceparent.as_bytes();
    let separators = [bytes[2], bytes[35], bytes[52]];
    if separators != *b"---"
        || bytes[0..2] == *b"ff"
        || !is_hex(&bytes[0..2])
        || !is_hex(&bytes[3..35])
        || !is_hex(&bytes[36..52])
        || !is_hex(&bytes[53..55])
        || bytes[3..35].iter().all(|byte| *byte == b'0')
        || bytes[36..52].iter().all(|byte| *byte == b'0')
    {
        tracing::warn!("ignoring malformed or oversized MCP trace context");
        return None;
    }

    Some(RemoteTrace {
        trace_id: traceparent[3..35].to_owned(),
        span_id: traceparent[36..52].to_owned(),
    })
}

fn is_hex(value: &[u8]) -> bool {
    value.iter().all(u8::is_ascii_hexdigit)
}

fn resource_label(uri: &str) -> String {
    uri.split_once(':')
        .map_or_else(|| "resource".to_owned(), |(scheme, _)| scheme.to_owned())
}

fn remote_context(meta: &rmcp::model::RequestMetaObject) -> opentelemetry::Context {
    let mut carrier = HashMap::new();
    if let Some(value) = meta.get_traceparent() {
        carrier.insert("traceparent".to_owned(), value.to_owned());
    }
    if let Some(value) = meta.get_tracestate() {
        carrier.insert("tracestate".to_owned(), value.to_owned());
    }
    if let Some(value) = meta.get_baggage() {
        carrier.insert("baggage".to_owned(), value.to_owned());
    }
    TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ])
    .extract(&carrier)
}

struct RequestSpan {
    span: tracing::Span,
    started: Instant,
    cancellation: CancellationToken,
}

impl RequestSpan {
    fn new(
        method: &'static str,
        transport: &'static str,
        context: &RequestContext<RoleServer>,
        tool: Option<&str>,
        resource: Option<&str>,
    ) -> Self {
        let span = tracing::info_span!(
            "mcp.request",
            request_id = %context.id,
            method,
            tool = tracing::field::Empty,
            resource = tracing::field::Empty,
            protocol_version = tracing::field::Empty,
            transport,
            duration_ms = tracing::field::Empty,
            result_bytes = tracing::field::Empty,
            inline_bytes = tracing::field::Empty,
            deferred_bytes = tracing::field::Empty,
            item_count = tracing::field::Empty,
            truncated = tracing::field::Empty,
            paginated = tracing::field::Empty,
            cache_hit = tracing::field::Empty,
            serialization_ms = tracing::field::Empty,
            actor_queue_ms = tracing::field::Empty,
            actor_execution_ms = tracing::field::Empty,
            cancelled = tracing::field::Empty,
            success = tracing::field::Empty,
            protocol_error = tracing::field::Empty,
            tool_error = tracing::field::Empty,
            trace_id = tracing::field::Empty,
            parent_span_id = tracing::field::Empty,
        );
        if let Some(tool) = tool {
            span.record("tool", tool);
        }
        if let Some(resource) = resource {
            span.record("resource", resource);
        }
        if let Some(protocol_version) = context.protocol_version() {
            span.record("protocol_version", protocol_version.as_str());
        }
        let remote = parse_remote_trace(&context.meta);
        if let Some(remote) = &remote {
            span.record("trace_id", remote.trace_id.as_str());
            span.record("parent_span_id", remote.span_id.as_str());
        }
        if remote.is_some() {
            let _ = span.set_parent(remote_context(&context.meta));
        }
        Self {
            span,
            started: Instant::now(),
            cancellation: context.ct.clone(),
        }
    }

    fn finish<T>(
        &self,
        result: &Result<T, ErrorData>,
        tool_error: bool,
        result_bytes: Option<usize>,
        result_metrics: Option<ToolResultMetrics>,
        serialization_started: Option<Instant>,
    ) {
        if let Some(result_bytes) = result_bytes {
            self.span.record("result_bytes", result_bytes);
            self.span.record("inline_bytes", result_bytes);
        }
        if let Some(result_metrics) = result_metrics {
            self.span
                .record("deferred_bytes", result_metrics.deferred_bytes);
            self.span.record("item_count", result_metrics.item_count);
            self.span.record("truncated", result_metrics.truncated);
            self.span.record("paginated", result_metrics.paginated);
            if let Some(cache_hit) = result_metrics.cache_hit {
                self.span.record("cache_hit", cache_hit);
            }
        }
        if let Some(serialization_started) = serialization_started {
            self.span.record(
                "serialization_ms",
                serialization_started.elapsed().as_secs_f64() * 1_000.0,
            );
        }
        self.span.record(
            "duration_ms",
            self.started.elapsed().as_secs_f64() * 1_000.0,
        );
        self.span
            .record("cancelled", self.cancellation.is_cancelled());
        self.span.record("success", result.is_ok() && !tool_error);
        self.span.record("protocol_error", result.is_err());
        self.span.record("tool_error", tool_error);
    }
}

async fn dispatch<T, F, Fut>(
    transport: &'static str,
    method: &'static str,
    context: RequestContext<RoleServer>,
    tool: Option<String>,
    resource: Option<String>,
    handler: F,
    tool_error: impl FnOnce(&T) -> bool,
) -> Result<T, ErrorData>
where
    F: FnOnce(RequestContext<RoleServer>) -> Fut,
    Fut: Future<Output = Result<T, ErrorData>>,
{
    let span = RequestSpan::new(
        method,
        transport,
        &context,
        tool.as_deref(),
        resource.as_deref(),
    );
    let result = handler(context).instrument(span.span.clone()).await;
    let is_tool_error = result.as_ref().is_ok_and(tool_error);
    span.finish(&result, is_tool_error, None, None, None);
    result
}

/// A `ServerHandler` forwarding wrapper that instruments every MCP request.
#[derive(Clone)]
pub struct InstrumentedServer<H> {
    inner: H,
    transport: &'static str,
}

impl<H> InstrumentedServer<H> {
    pub(crate) const fn new(inner: H, transport: &'static str) -> Self {
        Self { inner, transport }
    }
}

impl<H: ServerHandler> ServerHandler for InstrumentedServer<H> {
    fn ping(
        &self,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "ping",
            context,
            None,
            None,
            |context| self.inner.ping(context),
            |_| false,
        )
    }

    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "initialize",
            context,
            None,
            None,
            |context| self.inner.initialize(request, context),
            |_| false,
        )
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [rmcp::model::ProtocolVersion]> {
        self.inner.supported_protocol_versions()
    }

    fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "discover",
            context,
            None,
            None,
            |context| self.inner.discover(context),
            |_| false,
        )
    }

    fn complete(
        &self,
        request: CompleteRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CompleteResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "completion/complete",
            context,
            None,
            None,
            |context| self.inner.complete(request, context),
            |_| false,
        )
    }

    fn set_level(
        &self,
        request: SetLevelRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "logging/setLevel",
            context,
            None,
            None,
            |context| self.inner.set_level(request, context),
            |_| false,
        )
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetPromptResponse, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "prompts/get",
            context,
            None,
            None,
            |context| self.inner.get_prompt(request, context),
            |_| false,
        )
    }

    fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "prompts/list",
            context,
            None,
            None,
            |context| self.inner.list_prompts(request, context),
            |_| false,
        )
    }

    fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "resources/list",
            context,
            None,
            None,
            |context| self.inner.list_resources(request, context),
            |_| false,
        )
    }

    fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "resources/templates/list",
            context,
            None,
            None,
            |context| self.inner.list_resource_templates(request, context),
            |_| false,
        )
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, ErrorData>> + Send + '_ {
        let resource = resource_label(&request.uri);
        dispatch(
            self.transport,
            "resources/read",
            context,
            None,
            Some(resource),
            |context| self.inner.read_resource(request, context),
            |_| false,
        )
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        self.inner.accepted_subscription_filter(requested)
    }

    fn listen(
        &self,
        context: SubscriptionContext,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        let request_context = context.request_context().clone();
        let span = RequestSpan::new(
            "subscriptions/listen",
            self.transport,
            &request_context,
            None,
            None,
        );
        async move {
            let result = self
                .inner
                .listen(context)
                .instrument(span.span.clone())
                .await;
            span.finish(&result, false, None, None, None);
            result
        }
    }

    fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        let resource = resource_label(&request.uri);
        dispatch(
            self.transport,
            "resources/subscribe",
            context,
            None,
            Some(resource),
            |context| self.inner.subscribe(request, context),
            |_| false,
        )
    }

    fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        let resource = resource_label(&request.uri);
        dispatch(
            self.transport,
            "resources/unsubscribe",
            context,
            None,
            Some(resource),
            |context| self.inner.unsubscribe(request, context),
            |_| false,
        )
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + Send + '_ {
        let tool = request.name.clone();
        let span = RequestSpan::new(
            "tools/call",
            self.transport,
            &context,
            Some(tool.as_ref()),
            None,
        );
        async move {
            let result = self
                .inner
                .call_tool(request, context)
                .instrument(span.span.clone())
                .await;
            let tool_error = result.as_ref().is_ok_and(|response| match response {
                CallToolResponse::Complete(result) => result.is_error.unwrap_or(false),
                _ => false,
            });
            let serialization_started = Instant::now();
            let result_bytes = completed_tool_result_bytes(&result);
            let result_metrics = completed_tool_result_metrics(&result);
            span.finish(
                &result,
                tool_error,
                result_bytes,
                result_metrics,
                result_bytes.map(|_| serialization_started),
            );
            result
        }
    }

    fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "tools/list",
            context,
            None,
            None,
            |context| self.inner.list_tools(request, context),
            |_| false,
        )
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.inner.get_tool(name)
    }

    fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CustomResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "custom",
            context,
            None,
            None,
            |context| self.inner.on_custom_request(request, context),
            |_| false,
        )
    }

    fn on_cancelled(
        &self,
        notification: rmcp::model::CancelledNotificationParam,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.inner.on_cancelled(notification, context)
    }

    fn on_progress(
        &self,
        notification: rmcp::model::ProgressNotificationParam,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.inner.on_progress(notification, context)
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.inner.on_initialized(context)
    }

    fn on_roots_list_changed(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.inner.on_roots_list_changed(context)
    }

    fn on_custom_notification(
        &self,
        notification: rmcp::model::CustomNotification,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.inner.on_custom_notification(notification, context)
    }

    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }

    fn get_task(
        &self,
        request: GetTaskParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetTaskResult, ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "tasks/get",
            context,
            None,
            None,
            |context| self.inner.get_task(request, context),
            |_| false,
        )
    }

    fn update_task(
        &self,
        request: rmcp::model::UpdateTaskParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "tasks/update",
            context,
            None,
            None,
            |context| self.inner.update_task(request, context),
            |_| false,
        )
    }

    fn cancel_task(
        &self,
        request: rmcp::model::CancelTaskParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), ErrorData>> + Send + '_ {
        dispatch(
            self.transport,
            "tasks/cancel",
            context,
            None,
            None,
            |context| self.inner.cancel_task(request, context),
            |_| false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_traceparent_and_rejects_zero_ids() {
        let mut meta = rmcp::model::RequestMetaObject::new();
        meta.set_traceparent("00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01");
        assert_eq!(
            parse_remote_trace(&meta),
            Some(RemoteTrace {
                trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
                span_id: "00f067aa0ba902b7".into(),
            })
        );

        meta.set_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01");
        assert_eq!(parse_remote_trace(&meta), None);
    }

    #[test]
    fn ignores_malformed_and_oversized_trace_context_without_exposing_values() {
        let mut meta = rmcp::model::RequestMetaObject::new();
        meta.set_traceparent("not-a-traceparent");
        meta.set_baggage("secret=do-not-log");
        assert_eq!(parse_remote_trace(&meta), None);

        meta.set_traceparent("00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01");
        meta.set_tracestate("x".repeat(MAX_TRACESTATE_BYTES + 1));
        assert_eq!(parse_remote_trace(&meta), None);
    }

    #[test]
    fn resource_labels_drop_uri_paths() {
        assert_eq!(resource_label("file:///secret/workspace/main.rs"), "file");
        assert_eq!(resource_label("resource"), "resource");
    }

    #[test]
    fn completed_tool_result_bytes_count_payload_without_retaining_it() {
        let payload = rmcp::model::CallToolResult::success(Vec::new());
        let result = Ok::<_, ErrorData>(CallToolResponse::Complete(payload.clone()));

        assert_eq!(
            completed_tool_result_bytes(&result),
            Some(serde_json::to_vec(&payload).unwrap().len())
        );
    }

    #[test]
    fn completed_tool_result_metrics_exposes_only_response_shape_metadata() {
        let mut payload = rmcp::model::CallToolResult::success(Vec::new());
        payload.structured_content = Some(serde_json::json!({
            "cache_hit": true,
            "returned": 3,
            "truncated": true,
            "next_cursor": "3",
            "source": "must never reach telemetry",
            "resource": {
                "uri": "mcpls-source:///workspace/src.rs",
                "kind": "source_context",
                "total_bytes": 55
            }
        }));
        let result = Ok::<_, ErrorData>(CallToolResponse::Complete(payload));

        assert_eq!(
            completed_tool_result_metrics(&result),
            Some(ToolResultMetrics {
                cache_hit: Some(true),
                item_count: 3,
                deferred_bytes: 55,
                truncated: true,
                paginated: true,
            })
        );
    }

    #[test]
    fn completed_tool_result_metrics_reads_nested_diagnostics_cache_hit() {
        let mut payload = rmcp::model::CallToolResult::success(Vec::new());
        payload.structured_content = Some(serde_json::json!({
            "diagnostics": [],
            "cache": {"hit": true, "age_ms": 1, "snapshot_identity": "opaque"}
        }));
        let result = Ok::<_, ErrorData>(CallToolResponse::Complete(payload));

        assert_eq!(
            completed_tool_result_metrics(&result).unwrap().cache_hit,
            Some(true)
        );
    }

    #[test]
    fn completed_tool_result_metrics_prefers_reported_diagnostic_count() {
        let mut payload = rmcp::model::CallToolResult::success(Vec::new());
        payload.structured_content = Some(serde_json::json!({
            "diagnostics": [{"occurrence_count": 4}],
            "returned_diagnostics": 4
        }));
        let result = Ok::<_, ErrorData>(CallToolResponse::Complete(payload));

        assert_eq!(
            completed_tool_result_metrics(&result).unwrap().item_count,
            4
        );
    }

    #[test]
    fn completed_tool_result_metrics_reads_other_explicit_result_counts() {
        for (field, expected) in [
            ("returned_references", 2),
            ("returned_calls", 3),
            ("returned_groups", 4),
        ] {
            let mut payload = rmcp::model::CallToolResult::success(Vec::new());
            payload.structured_content = Some(serde_json::json!({field: expected}));
            let result = Ok::<_, ErrorData>(CallToolResponse::Complete(payload));

            assert_eq!(
                completed_tool_result_metrics(&result).unwrap().item_count,
                expected
            );
        }
    }
}
