//! Assorted middleware that implements LSP server semantics.
use std::marker::PhantomData;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use std::pin::Pin;
use std::fmt;
use std::error::Error as ErrorType;


use futures::future::{self, Future, BoxFuture, FutureExt};
use tower::{Layer, Service};
use tracing::{info, warn};

use super::ExitedError;
use crate::jsonrpc::{not_initialized_error, Error, Id, Request, Response};

use super::client::Client;
use super::pending::Pending;
use super::state::{ServerState, State};

/// Middleware which implements `initialize` request semantics.
///
/// # Specification
///
/// https://microsoft.github.io/language-server-protocol/specification#initialize
pub struct Initialize {
    state: Arc<ServerState>,
    pending: Arc<Pending>,
}

impl Initialize {
    pub fn new(state: Arc<ServerState>, pending: Arc<Pending>) -> Self {
        Initialize { state, pending }
    }
}

impl<S> Layer<S> for Initialize {
    type Service = InitializeService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        InitializeService {
            inner: Cancellable::new(inner, self.pending.clone()),
            state: self.state.clone(),
        }
    }
}

/// Service created from [`Initialize`] layer.
pub struct InitializeService<S> {
    inner: Cancellable<S>,
    state: Arc<ServerState>,
}

impl<S> Service<Request> for InitializeService<S>
where
    S: Service<Request, Response = Option<Response>, Error = ExitedError>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        if self.state.get() == State::Uninitialized {
            let state = self.state.clone();
            let fut = self.inner.call(req);

            Box::pin(async move {
                let response = fut.await?;

                match &response {
                    Some(res) if res.is_ok() => state.set(State::Initialized),
                    _ => state.set(State::Uninitialized),
                }

                Ok(response)
            })
        } else {
            warn!("received duplicate `initialize` request, ignoring");
            let (_, id, _) = req.into_parts();
            future::ok(id.map(|id| Response::from_error(id, Error::invalid_request()))).boxed()
        }
    }
}

/// Middleware which implements `shutdown` request semantics.
///
/// # Specification
///
/// https://microsoft.github.io/language-server-protocol/specification#shutdown
pub struct Shutdown {
    state: Arc<ServerState>,
    pending: Arc<Pending>,
}

impl Shutdown {
    pub fn new(state: Arc<ServerState>, pending: Arc<Pending>) -> Self {
        Shutdown { state, pending }
    }
}

impl<S> Layer<S> for Shutdown {
    type Service = ShutdownService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ShutdownService {
            inner: Cancellable::new(inner, self.pending.clone()),
            state: self.state.clone(),
        }
    }
}

/// Service created from [`Shutdown`] layer.
pub struct ShutdownService<S> {
    inner: Cancellable<S>,
    state: Arc<ServerState>,
}

impl<S> Service<Request> for ShutdownService<S>
where
    S: Service<Request, Response = Option<Response>, Error = ExitedError>,
    S::Future: Into<BoxFuture<'static, Result<Option<Response>, S::Error>>> + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        match self.state.get() {
            State::Initialized => {
                info!("shutdown request received, shutting down");
                self.state.set(State::ShutDown);
                self.inner.call(req)
            }
            cur_state => {
                let (_, id, _) = req.into_parts();
                future::ok(not_initialized_response(id, cur_state)).boxed()
            }
        }
    }
}

/// Middleware which implements `exit` notification semantics.
///
/// # Specification
///
/// https://microsoft.github.io/language-server-protocol/specification#exit
pub struct Exit {
    state: Arc<ServerState>,
    pending: Arc<Pending>,
    client: Client,
}

impl Exit {
    pub fn new(state: Arc<ServerState>, pending: Arc<Pending>, client: Client) -> Self {
        Exit {
            state,
            pending,
            client,
        }
    }
}

impl<S> Layer<S> for Exit {
    type Service = ExitService<S>;

    fn layer(&self, _: S) -> Self::Service {
        ExitService {
            state: self.state.clone(),
            pending: self.pending.clone(),
            client: self.client.clone(),
            _marker: PhantomData,
        }
    }
}

/// Service created from [`Exit`] layer.
pub struct ExitService<S> {
    state: Arc<ServerState>,
    pending: Arc<Pending>,
    client: Client,
    _marker: PhantomData<S>,
}

impl<S> Service<Request> for ExitService<S> {
    type Response = Option<Response>;
    type Error = ExitedError;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.state.get() == State::Exited {
            Poll::Ready(Err(ExitedError(())))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn call(&mut self, _: Request) -> Self::Future {
        info!("exit notification received, stopping");
        self.state.set(State::Exited);
        self.pending.cancel_all();
        self.client.close();
        future::ok(None)
    }
}

/// Middleware which implements LSP semantics for all other kinds of requests.
pub struct Normal {
    state: Arc<ServerState>,
    pending: Arc<Pending>,
}

impl Normal {
    pub fn new(state: Arc<ServerState>, pending: Arc<Pending>) -> Self {
        Normal { state, pending }
    }
}

impl<S> Layer<S> for Normal {
    type Service = NormalService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NormalService {
            inner: Cancellable::new(inner, self.pending.clone()),
            state: self.state.clone(),
        }
    }
}

/// Service created from [`Normal`] layer.
pub struct NormalService<S> {
    inner: Cancellable<S>,
    state: Arc<ServerState>,
}

impl<S> Service<Request> for NormalService<S>
where
    S: Service<Request, Response = Option<Response>, Error = ExitedError>,
    S::Future: Into<BoxFuture<'static, Result<Option<Response>, S::Error>>> + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        match self.state.get() {
            State::Initialized => self.inner.call(req),
            cur_state => {
                let (_, id, _) = req.into_parts();
                future::ok(not_initialized_response(id, cur_state)).boxed()
            }
        }
    }
}

/// Wraps an inner service `S` and implements `$/cancelRequest` semantics for all requests.
///
/// # Specification
///
/// https://microsoft.github.io/language-server-protocol/specification#cancelRequest
struct Cancellable<S> {
    inner: S,
    pending: Arc<Pending>,
}

impl<S> Cancellable<S> {
    fn new(inner: S, pending: Arc<Pending>) -> Self {
        Cancellable { inner, pending }
    }
}

impl<S> Service<Request> for Cancellable<S>
where
    S: Service<Request, Response = Option<Response>, Error = ExitedError>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        match req.id().cloned() {
            Some(id) => self.pending.execute(id, self.inner.call(req)).boxed(),
            None => self.inner.call(req).boxed(),
        }
    }
}

fn not_initialized_response(id: Option<Id>, server_state: State) -> Option<Response> {
    let id = id?;
    let error = match server_state {
        State::Uninitialized | State::Initializing => not_initialized_error(),
        _ => Error::invalid_request(),
    };

    Some(Response::from_error(id, error))
}

// TODO: Add some `tower-test` middleware tests for each middleware.

// Our timeout service, which wraps another service and
// adds a timeout to its response future.
pub struct Timeout<T> {
  inner: T,
  timeout: Duration,
}

impl<T> Timeout<T> {
  pub fn new(inner: T, timeout: Duration) -> Timeout<T> {
    Timeout {
      inner,
      timeout
    }
  }
}

// The error returned if processing a request timed out
#[derive(Debug)]
pub struct Expired;

impl fmt::Display for Expired {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "expired")
  }
}

impl ErrorType for Expired {}

// We can implement `Service` for `Timeout<T>` if `T` is a `Service`
impl<T, Request> Service<Request> for Timeout<T>
where
T: Service<Request>,
T::Future: 'static,
T::Error: Into<Box<dyn ErrorType + Send + Sync>> + 'static,
T::Response: 'static,
{
  // `Timeout` doesn't modify the response type, so we use `T`'s response type
  type Response = T::Response;
  // Errors may be either `Expired` if the timeout expired, or the inner service's
  // `Error` type. Therefore, we return a boxed `dyn Error + Send + Sync` trait object to erase
  // the error's type.
  type Error = Box<dyn ErrorType + Send + Sync>;
  type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

  fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    // Our timeout service is ready if the inner service is ready.
    // This is how backpressure can be propagated through a tree of nested services.
    self.inner.poll_ready(cx).map_err(Into::into)
  }

  fn call(&mut self, req: Request) -> Self::Future {
    // Create a future that completes after `self.timeout`
    let timeout = tokio::time::sleep(self.timeout);

    // Call the inner service and get a future that resolves to the response
    let fut = self.inner.call(req);

    // Wrap those two futures in another future that completes when either one completes
    // If the inner service is too slow the `sleep` future will complete first
    // And an error will be returned and `fut` will be dropped and not polled again
    //
    // We have to box the errors so the types match
    let f = async move {
      tokio::select! {
        res = fut => {
          res.map_err(|err| err.into())
        },
        _ = timeout => {
          Err(Box::new(Expired) as Box<dyn ErrorType + Send + Sync>)
        },
      }
    };

    Box::pin(f)
  }
}

// A layer for wrapping services in `Timeout`
pub struct TimeoutLayer(Duration);

impl TimeoutLayer {
  pub fn new(delay: Duration) -> Self {
    TimeoutLayer(delay)
  }
}

impl<S> Layer<S> for TimeoutLayer {
  type Service = Timeout<S>;

  fn layer(&self, service: S) -> Timeout<S> {
    Timeout::new(service, self.0)
  }
}
