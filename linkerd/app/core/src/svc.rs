// Possibly unused, but useful during development.

pub use crate::proxy::http;
use crate::{cache, Error};
pub use linkerd_concurrency_limit::ConcurrencyLimit;
pub use linkerd_stack::{
    self as stack, layer, BoxNewService, BoxService, BoxServiceLayer, Fail, Filter, MapTargetLayer,
    NewRouter, NewService, Param, Predicate, UnwrapOr,
};
pub use linkerd_stack_tracing::{NewInstrument, NewInstrumentLayer};
pub use linkerd_timeout::{self as timeout, FailFast};
use std::{
    task::{Context, Poll},
    time::Duration,
};
use tower::{
    buffer::{Buffer as TowerBuffer, BufferLayer},
    layer::util::{Identity, Stack as Pair},
    make::MakeService,
};
pub use tower::{
    layer::Layer,
    service_fn as mk,
    spawn_ready::SpawnReady,
    util::{Either, MapErrLayer},
    Service, ServiceExt,
};

pub type Buffer<Req, Rsp, E> = TowerBuffer<BoxService<Req, Rsp, E>, Req>;

#[derive(Clone, Debug)]
pub struct Layers<L>(L);

#[derive(Clone, Debug)]
pub struct Stack<S>(S);

pub fn layers() -> Layers<Identity> {
    Layers(Identity::new())
}

pub fn stack<S>(inner: S) -> Stack<S> {
    Stack(inner)
}

pub fn proxies() -> Stack<IdentityProxy> {
    Stack(IdentityProxy(()))
}

#[derive(Copy, Clone, Debug)]
pub struct IdentityProxy(());

impl<T> NewService<T> for IdentityProxy {
    type Service = ();
    fn new_service(&mut self, _: T) -> Self::Service {}
}

#[allow(dead_code)]
impl<L> Layers<L> {
    pub fn push<O>(self, outer: O) -> Layers<Pair<L, O>> {
        Layers(Pair::new(self.0, outer))
    }

    pub fn push_map_target<M>(self, map_target: M) -> Layers<Pair<L, stack::MapTargetLayer<M>>> {
        self.push(stack::MapTargetLayer::new(map_target))
    }

    /// Wraps an inner `MakeService` to be a `NewService`.
    pub fn push_into_new_service(
        self,
    ) -> Layers<Pair<L, stack::new_service::FromMakeServiceLayer>> {
        self.push(stack::new_service::FromMakeServiceLayer::default())
    }

    /// Buffers requests in an mpsc, spawning the inner service onto a dedicated task.
    pub fn push_spawn_buffer<Req>(
        self,
        capacity: usize,
    ) -> Layers<Pair<Pair<L, BoxServiceLayer<Req>>, BufferLayer<Req>>>
    where
        Req: Send + 'static,
    {
        self.push(BoxServiceLayer::new())
            .push(BufferLayer::new(capacity))
    }

    pub fn push_on_response<U>(self, layer: U) -> Layers<Pair<L, stack::OnResponseLayer<U>>> {
        self.push(stack::OnResponseLayer::new(layer))
    }

    pub fn push_instrument<G: Clone>(self, get_span: G) -> Layers<Pair<L, NewInstrumentLayer<G>>> {
        self.push(NewInstrumentLayer::new(get_span))
    }
}

impl<M, L: Layer<M>> Layer<M> for Layers<L> {
    type Service = L::Service;

    fn layer(&self, inner: M) -> Self::Service {
        self.0.layer(inner)
    }
}

#[allow(dead_code)]
impl<S> Stack<S> {
    pub fn push<L: Layer<S>>(self, layer: L) -> Stack<L::Service> {
        Stack(layer.layer(self.0))
    }

    pub fn push_map_target<M: Clone>(self, map_target: M) -> Stack<stack::MapTargetService<S, M>> {
        self.push(stack::MapTargetLayer::new(map_target))
    }

    pub fn push_request_filter<F: Clone>(self, filter: F) -> Stack<stack::Filter<S, F>> {
        self.push(stack::Filter::<S, F>::layer(filter))
    }

    /// Wraps a `Service<T>` as a `Service<()>`.
    ///
    /// Each time the service is called, the `T`-typed request is cloned and
    /// issued into the inner service.
    pub fn push_make_thunk(self) -> Stack<stack::MakeThunk<S>> {
        self.push(layer::mk(stack::MakeThunk::new))
    }

    pub fn instrument<G: Clone>(self, get_span: G) -> Stack<NewInstrument<G, S>> {
        self.push(NewInstrumentLayer::new(get_span))
    }

    pub fn instrument_from_target(self) -> Stack<NewInstrument<(), S>> {
        self.push(NewInstrumentLayer::from_target())
    }

    /// Wraps an inner `MakeService` to be a `NewService`.
    pub fn into_new_service(self) -> Stack<stack::new_service::FromMakeService<S>> {
        self.push(stack::new_service::FromMakeServiceLayer::default())
    }

    /// Buffer requests when when the next layer is out of capacity.
    pub fn spawn_buffer<Req, Rsp>(
        self,
        capacity: usize,
    ) -> Stack<Buffer<Req, S::Response, S::Error>>
    where
        Req: Send + 'static,
        S: Service<Req> + Send + 'static,
        S::Response: Send + 'static,
        S::Error: Into<Error> + Send + Sync + 'static,
        S::Future: Send,
    {
        self.push(BoxServiceLayer::new())
            .push(BufferLayer::new(capacity))
    }

    /// Assuming `S` implements `NewService` or `MakeService`, applies the given
    /// `L`-typed layer on each service produced by `S`.
    pub fn push_on_response<L: Clone>(self, layer: L) -> Stack<stack::OnResponse<L, S>> {
        self.push(stack::OnResponseLayer::new(layer))
    }

    pub fn push_timeout(self, timeout: Duration) -> Stack<tower::timeout::Timeout<S>> {
        self.push(tower::timeout::TimeoutLayer::new(timeout))
    }

    pub fn push_http_insert_target<P>(self) -> Stack<http::insert::NewInsert<P, S>> {
        self.push(http::insert::NewInsert::layer())
    }

    pub fn push_cache<T>(self, idle: Duration) -> Stack<cache::Cache<T, S>>
    where
        T: Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
        S: NewService<T> + 'static,
        S::Service: Send + Sync + 'static,
    {
        self.push(cache::Cache::layer(idle))
    }

    /// Push a service that either calls the inner service if it is ready, or
    /// calls a `secondary` service if the inner service fails to become ready
    /// for the `skip_after` duration.
    pub fn push_when_unready<B: Clone>(
        self,
        skip_after: Duration,
        secondary: B,
    ) -> Stack<stack::NewSwitchReady<S, B>> {
        self.push(layer::mk(|inner: S| {
            stack::NewSwitchReady::new(inner, secondary.clone(), skip_after)
        }))
    }

    pub fn push_switch<P: Clone, U: Clone>(
        self,
        predicate: P,
        other: U,
    ) -> Stack<Filter<stack::NewEither<S, U>, P>> {
        self.push(layer::mk(move |inner| {
            stack::Filter::new(
                stack::NewEither::new(inner, other.clone()),
                predicate.clone(),
            )
        }))
    }

    /// Validates that this stack serves T-typed targets.
    #[track_caller]
    pub fn check_new<T>(self) -> Self
    where
        S: NewService<T>,
    {
        #[cfg(dump_stacks)]
        {
            use std::any::type_name;

            let s_name = type_name::<S>();
            let svc_name = type_name::<S::Service>();
            eprintln!(
                "\ncheck_new::<{}>\n  at {}",
                type_name::<T>(),
                std::panic::Location::caller()
            );
            eprintln!(
                "  new service: {:>5} chars, {:>5} angles",
                s_name.len(),
                s_name.matches('<').count()
            );
            eprintln!(
                "      service: {:>5} chars, {:>5} angles",
                svc_name.len(),
                svc_name.matches('<').count()
            );
        }
        self
    }

    #[track_caller]
    pub fn check_new_clone<T>(self) -> Self
    where
        S: NewService<T>,
        S::Service: Clone,
    {
        #[cfg(dump_stacks)]
        {
            use std::any::type_name;

            let s_name = type_name::<S>();
            let svc_name = type_name::<S::Service>();
            eprintln!(
                "\ncheck_new_clone::<{}>\n  at {}",
                type_name::<T>(),
                std::panic::Location::caller()
            );
            eprintln!(
                "  new service: {:>5} chars, {:>5} angles",
                s_name.len(),
                s_name.matches('<').count()
            );
            eprintln!(
                "      service: {:>5} chars, {:>5} angles",
                svc_name.len(),
                svc_name.matches('<').count()
            );
        }
        self
    }

    /// Validates that this stack serves T-typed targets.
    #[track_caller]
    pub fn check_new_service<T, Req>(self) -> Self
    where
        S: NewService<T>,
        S::Service: Service<Req>,
    {
        #[cfg(dump_stacks)]
        {
            use std::any::type_name;

            let s_name = type_name::<S>();
            let svc_name = type_name::<S::Service>();
            let fut_name = type_name::<<S::Service as Service<Req>>::Future>();
            let rsp_name = type_name::<<S::Service as Service<Req>>::Response>();
            eprintln!(
                "\ncheck_new_service::<{}, {}>\n at {}",
                type_name::<T>(),
                type_name::<Req>(),
                std::panic::Location::caller()
            );
            eprintln!(
                "  new service: {:>5} chars, {:>5} angles",
                s_name.len(),
                s_name.matches('<').count()
            );
            eprintln!(
                "      service: {:>5} chars, {:>5} angles",
                svc_name.len(),
                svc_name.matches('<').count()
            );
            eprintln!(
                "       future: {:>5} chars, {:>5} angles",
                fut_name.len(),
                fut_name.matches('<').count()
            );
            eprintln!(
                "     response: {:>5} chars, {:>5} angles",
                rsp_name.len(),
                rsp_name.matches('<').count()
            );
        }
        self
    }

    /// Validates that this stack serves T-typed targets.
    #[track_caller]
    pub fn check_clone_new_service<T, Req>(self) -> Self
    where
        S: NewService<T> + Clone,
        S::Service: Service<Req>,
    {
        #[cfg(dump_stacks)]
        {
            use std::any::type_name;

            let s_name = type_name::<S>();
            let svc_name = type_name::<S::Service>();
            let fut_name = type_name::<<S::Service as Service<Req>>::Future>();
            let rsp_name = type_name::<<S::Service as Service<Req>>::Response>();
            eprintln!(
                "\ncheck_clone_new_service::<{}, {}>\n at {}",
                type_name::<T>(),
                type_name::<Req>(),
                std::panic::Location::caller()
            );
            eprintln!(
                "  new service: {:>5} chars, {:>5} angles",
                s_name.len(),
                s_name.matches('<').count()
            );
            eprintln!(
                "      service: {:>5} chars, {:>5} angles",
                svc_name.len(),
                svc_name.matches('<').count()
            );
            eprintln!(
                "       future: {:>5} chars, {:>5} angles",
                fut_name.len(),
                fut_name.matches('<').count()
            );
            eprintln!(
                "     response: {:>5} chars, {:>5} angles",
                rsp_name.len(),
                rsp_name.matches('<').count()
            );
        }
        self
    }

    /// Validates that this stack can be cloned
    pub fn check_clone(self) -> Self
    where
        S: Clone,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    #[track_caller]
    pub fn check_service<T>(self) -> Self
    where
        S: Service<T>,
    {
        #[cfg(dump_stacks)]
        {
            use std::any::type_name;

            let svc_name = type_name::<S>();
            let fut_name = type_name::<S::Future>();
            let rsp_name = type_name::<S::Response>();
            eprintln!(
                "\ncheck_service::<{}>\n at {}",
                type_name::<T>(),
                std::panic::Location::caller()
            );
            eprintln!(
                "      service: {:>5} chars, {:>5} angles",
                svc_name.len(),
                svc_name.matches('<').count()
            );
            eprintln!(
                "       future: {:>5} chars, {:>5} angles",
                fut_name.len(),
                fut_name.matches('<').count()
            );
            eprintln!(
                "     response: {:>5} chars, {:>5} angles",
                rsp_name.len(),
                rsp_name.matches('<').count()
            );
        }
        self
    }

    /// Validates that this stack serves T-typed targets with `Unpin` futures.
    #[track_caller]
    pub fn check_service_unpin<T>(self) -> Self
    where
        S: Service<T>,
        S::Future: Unpin,
    {
        #[cfg(dump_stacks)]
        {
            use std::any::type_name;

            let svc_name = type_name::<S>();
            let fut_name = type_name::<S::Future>();
            let rsp_name = type_name::<S::Response>();
            eprintln!(
                "\ncheck_service_unpin::<{}>\n at {}",
                type_name::<T>(),
                std::panic::Location::caller()
            );
            eprintln!(
                "      service: {:>5} chars, {:>5} angles",
                svc_name.len(),
                svc_name.matches('<').count()
            );
            eprintln!(
                "       future: {:>5} chars, {:>5} angles",
                fut_name.len(),
                fut_name.matches('<').count()
            );
            eprintln!(
                "     response: {:>5} chars, {:>5} angles",
                rsp_name.len(),
                rsp_name.matches('<').count()
            );
        }
        self
    }

    #[track_caller]
    pub fn check_service_response<T, U>(self) -> Self
    where
        S: Service<T, Response = U>,
    {
        #[cfg(dump_stacks)]
        {
            use std::any::type_name;

            let svc_name = type_name::<S>();
            let fut_name = type_name::<S::Future>();
            let rsp_name = type_name::<U>();
            eprintln!(
                "\ncheck_service_response::<{}, {}>\n at {}",
                type_name::<T>(),
                rsp_name,
                std::panic::Location::caller()
            );
            eprintln!(
                "      service: {:>5} chars, {:>5} angles",
                svc_name.len(),
                svc_name.matches('<').count()
            );
            eprintln!(
                "       future: {:>5} chars, {:>5} angles",
                fut_name.len(),
                fut_name.matches('<').count()
            );
            eprintln!(
                "     response: {:>5} chars, {:>5} angles",
                rsp_name.len(),
                rsp_name.matches('<').count()
            );
        }
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_make_service<T, U>(self) -> Self
    where
        S: MakeService<T, U>,
    {
        self
    }

    /// Validates that this stack serves T-typed targets.
    pub fn check_make_service_clone<T, U>(self) -> Self
    where
        S: MakeService<T, U> + Clone,
        S::Service: Clone,
    {
        self
    }

    pub fn check_new_send_and_static<M, T, Req>(self) -> Self
    where
        S: NewService<T, Service = M>,
        M: Service<Req> + Send + 'static,
        M::Response: Send + 'static,
        M::Error: Into<Error> + Send + Sync,
        M::Future: Send,
    {
        self
    }

    pub fn into_inner(self) -> S {
        self.0
    }
}

impl<T, N> NewService<T> for Stack<N>
where
    N: NewService<T>,
{
    type Service = N::Service;

    fn new_service(&mut self, t: T) -> Self::Service {
        self.0.new_service(t)
    }
}

impl<T, S> Service<T> for Stack<S>
where
    S: Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, t: T) -> Self::Future {
        self.0.call(t)
    }
}
