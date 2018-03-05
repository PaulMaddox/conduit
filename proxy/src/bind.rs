use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use http::{self, uri};
use tokio_core::reactor::Handle;
use tower;
use tower_h2;
use tower_reconnect::Reconnect;

use conduit_proxy_controller_grpc;
use conduit_proxy_router::Uses;
use control;
use ctx;
use telemetry::{self, sensor};
use transparency::{self, HttpBody, h1};
use transport;
use ::timeout::Timeout;

const DEFAULT_TIMEOUT_MS: u64 = 300;

/// Binds a `Service` from a `SocketAddr`.
///
/// The returned `Service` buffers request until a connection is established.
///
/// # TODO
///
/// Buffering is not bounded and no timeouts are applied.
pub struct Bind<C, B> {
    ctx: C,
    sensors: telemetry::Sensors,
    executor: Handle,
    req_ids: Arc<AtomicUsize>,
    connect_timeout: Duration,
    _p: PhantomData<B>,
}

/// Binds a `Service` from a `SocketAddr` for a pre-determined protocol.
pub struct BindProtocol<C, B> {
    bind: Bind<C, B>,
    protocol: Protocol,
}

/// Protocol portion of the `Recognize` key for a request.
///
/// This marks whether to use HTTP/2 or HTTP/1.x for a request. In
/// the case of HTTP/1.x requests, it also stores a "host" key to ensure
/// that each host receives its own connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Http1(Host),
    Http2
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    Authority(uri::Authority),
    NoAuthority,
}

pub type Service<B> = Reconnect<NewHttp<B>>;

pub type NewHttp<B> = sensor::NewHttp<Client<B>, B, HttpBody>;

pub type HttpResponse = http::Response<sensor::http::ResponseBody<HttpBody>>;

pub type Client<B> = transparency::Client<
    sensor::Connect<transport::TimeoutConnect<transport::Connect>>,
    B,
>;

impl<B> Bind<(), B> {
    pub fn new(executor: Handle) -> Self {
        Self {
            executor,
            ctx: (),
            sensors: telemetry::Sensors::null(),
            req_ids: Default::default(),
            connect_timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            _p: PhantomData,
        }
    }

    pub fn with_connect_timeout(self, connect_timeout: Duration) -> Self {
        Self {
            connect_timeout,
            ..self
        }
    }

    pub fn with_sensors(self, sensors: telemetry::Sensors) -> Self {
        Self {
            sensors,
            ..self
        }
    }

    pub fn with_ctx<C>(self, ctx: C) -> Bind<C, B> {
        Bind {
            ctx,
            sensors: self.sensors,
            executor: self.executor,
            req_ids: self.req_ids,
            connect_timeout: self.connect_timeout,
            _p: PhantomData,
        }
    }
}

impl<C: Clone, B> Clone for Bind<C, B> {
    fn clone(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
            sensors: self.sensors.clone(),
            executor: self.executor.clone(),
            req_ids: self.req_ids.clone(),
            connect_timeout: self.connect_timeout,
            _p: PhantomData,
        }
    }
}


impl<C, B> Bind<C, B> {
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    // pub fn ctx(&self) -> &C {
    //     &self.ctx
    // }

    pub fn executor(&self) -> &Handle {
        &self.executor
    }

    // pub fn req_ids(&self) -> &Arc<AtomicUsize> {
    //     &self.req_ids
    // }

    // pub fn sensors(&self) -> &telemetry::Sensors {
    //     &self.sensors
    // }

}

impl<B> Bind<Arc<ctx::Proxy>, B>
where
    B: tower_h2::Body + 'static,
{
    pub fn bind_service(&self, addr: &SocketAddr, protocol: &Protocol) -> Service<B> {
        trace!("bind_service addr={}, protocol={:?}", addr, protocol);
        let client_ctx = ctx::transport::Client::new(
            &self.ctx,
            addr,
            conduit_proxy_controller_grpc::common::Protocol::Http,
        );

        // Map a socket address to a connection.
        let connect = {
            let c = Timeout::new(
                transport::Connect::new(*addr, &self.executor),
                self.connect_timeout,
                &self.executor,
            );

            self.sensors.connect(c, &client_ctx)
        };

        let client = transparency::Client::new(
            protocol,
            connect,
            self.executor.clone(),
        );

        let proxy = self.sensors.http(self.req_ids.clone(), client, &client_ctx);

        // Automatically perform reconnects if the connection fails.
        //
        // TODO: Add some sort of backoff logic.
        Reconnect::new(proxy)
    }
}

// ===== impl BindProtocol =====


impl<C, B> Bind<C, B> {
    pub fn with_protocol(self, protocol: Protocol) -> BindProtocol<C, B> {
        BindProtocol {
            bind: self,
            protocol,
        }
    }
}

impl<B> control::discovery::Bind for BindProtocol<Arc<ctx::Proxy>, B>
where
    B: tower_h2::Body + 'static,
{
    type Request = http::Request<B>;
    type Response = HttpResponse;
    type Error = <Service<B> as tower::Service>::Error;
    type Service = Service<B>;
    type BindError = ();

    fn bind(&self, addr: &SocketAddr) -> Result<Self::Service, Self::BindError> {
        Ok(self.bind.bind_service(addr, &self.protocol))
    }
}

// ===== impl Protocol =====


impl<'a, B> From<&'a http::Request<B>> for Protocol {
    fn from(req: &'a http::Request<B>) -> Protocol {
        if req.version() == http::Version::HTTP_2 {
            return Protocol::Http2
        }

        if req.extensions().get::<h1::AuthorityRewriting>() ==
            Some(&h1::AuthorityRewriting::SoOriginalDst)
        {
            return Protocol::Http1(Host::NoAuthority);
        }

        // If the request has an authority part, use that as the host part of
        // the key for an HTTP/1.x request.
        let host = req.uri()
            .authority_part()
            .cloned()
            .map(Host::Authority)
            .unwrap_or_else(|| Host::NoAuthority);

        Protocol::Http1(host)
    }
}

impl Protocol {

    pub fn is_cachable(&self) -> bool {
        match *self {
            Protocol::Http2 | Protocol::Http1(Host::Authority(_)) => true,
            _ => false,
        }
    }

    pub fn into_key<T>(self, key: T) -> Uses<(T, Protocol)> {
        if self.is_cachable() {
            Uses::Reusable((key, self))
        } else {
            Uses::SingleUse((key, self))
        }
    }
}
