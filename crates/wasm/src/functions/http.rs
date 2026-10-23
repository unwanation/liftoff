//! One request through a function's `wasi:http/incoming-handler`: the
//! request in, the response (head and body) out.

use core::pin::pin;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use liftoff_core::domain::EnvironmentId;
use liftoff_core::ports::Store;
use tokio::sync::oneshot;
use wasmtime_wasi_http::WasiHttpView as _;
use wasmtime_wasi_http::p2::bindings::http::types::{ErrorCode, Scheme};
use wasmtime_wasi_http::p2::body::HyperOutgoingBody;

use super::FunctionError;
use super::pool::Instance;

/// The largest response body a function may answer; a larger one fails
/// the request (a trap), so a function cannot grow the daemon.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// What the guest handed its `response-outparam`.
type Head = Result<hyper::Response<HyperOutgoingBody>, ErrorCode>;

/// The host's failure to set up a request for the guest.
fn setup_failed(error: &wasmtime::Error) -> FunctionError {
    tracing::error!(error = ?error, "a request for a function could not be set up");
    FunctionError::HostFailed {
        reason: "the request could not be handed to the function".to_owned(),
    }
}

/// Calls `handle` of `instance` with `request` and reads the response it
/// sets, body and all.
pub(super) async fn exchange<S: Store + 'static>(
    environment: EnvironmentId,
    instance: &mut Instance<S>,
    request: http::Request<Bytes>,
) -> Result<http::Response<Bytes>, FunctionError> {
    let Instance { store, proxy } = instance;
    let (sender, mut receiver) = oneshot::channel::<Head>();
    let (incoming, outparam) = {
        let mut view = store.data_mut().http();
        let incoming = view
            .new_incoming_request(Scheme::Http, request.map(Full::new))
            .map_err(|error| setup_failed(&error))?;
        let outparam = view
            .new_response_outparam(sender)
            .map_err(|error| setup_failed(&error))?;
        (incoming, outparam)
    };
    let (done, finished) = oneshot::channel::<()>();
    let call = async {
        let called = proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut *store, incoming, outparam)
            .await;
        let _told: Result<(), ()> = done.send(());
        called
    };
    let answer = async {
        // A guest that returns without setting its response leaves the
        // sender in its table: the end of the call ends the wait.
        let head = tokio::select! {
            biased;
            head = &mut receiver => head.ok(),
            _ended = finished => receiver.try_recv().ok(),
        };
        match head {
            None => Err(FunctionError::Trap {
                reason: "the function set no response".to_owned(),
            }),
            Some(Err(code)) => Err(FunctionError::Trap {
                reason: format!("the function answered the error {code:?}"),
            }),
            Some(Ok(response)) => collect(response).await,
        }
    };
    let mut call = pin!(call);
    let mut answer = pin!(answer);
    let mut answered = None;
    // A trap ends the request at once, whatever the body did.
    let called = loop {
        tokio::select! {
            called = &mut call => break called,
            got = &mut answer, if answered.is_none() => answered = Some(got),
        }
    };
    called.map_err(|error| super::failure(environment, &error))?;
    match answered {
        Some(answered) => answered,
        None => answer.await,
    }
}

/// The response with its body read, at most [`MAX_RESPONSE_BYTES`].
async fn collect(
    response: hyper::Response<HyperOutgoingBody>,
) -> Result<http::Response<Bytes>, FunctionError> {
    let (parts, body) = response.into_parts();
    let body = Limited::new(body, MAX_RESPONSE_BYTES)
        .collect()
        .await
        .map_err(|error| FunctionError::Trap {
            reason: format!("the response body failed: {error}"),
        })?
        .to_bytes();
    Ok(http::Response::from_parts(parts, body))
}
