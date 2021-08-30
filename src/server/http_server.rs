use crate::server::{Accept, Handle, ListenerTask, MakeParts};
use crate::util::HyperService;

use std::io;
use std::net::SocketAddr;

use futures_util::future::Ready;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use tower_http::add_extension::AddExtension;
use tower_layer::Layer;

use hyper::server::conn::Http;
use hyper::Request;

#[derive(Clone)]
pub struct CloneParts<L, A> {
    layer: L,
    acceptor: A,
}

impl<L, A> CloneParts<L, A> {
    fn new(layer: L, acceptor: A) -> Self {
        Self { layer, acceptor }
    }
}

impl<L, A> MakeParts for CloneParts<L, A>
where
    L: Clone,
    A: Clone,
{
    type Layer = L;
    type Acceptor = A;

    fn make_parts(&self) -> (Self::Layer, Self::Acceptor) {
        (self.layer.clone(), self.acceptor.clone())
    }
}

#[derive(Debug)]
enum Mode {
    Shutdown,
    Graceful,
}

#[derive(Clone)]
pub(crate) struct HttpServer<S, M> {
    service: S,
    handle: Handle,
    make_parts: M,
}

impl<S, M> HttpServer<S, M> {
    pub fn new(service: S, handle: Handle, make_parts: M) -> Self {
        Self {
            service,
            handle,
            make_parts,
        }
    }
}

impl<S> HttpServer<S, CloneParts<NoopLayer, NoopAcceptor>> {
    pub fn from_service(service: S, handle: Handle) -> Self {
        HttpServer::new(service, handle, CloneParts::new(NoopLayer, NoopAcceptor))
    }
}

#[cfg(feature = "tls-rustls")]
impl<S, A> HttpServer<S, CloneParts<NoopLayer, A>> {
    pub fn from_acceptor(service: S, handle: Handle, acceptor: A) -> Self {
        HttpServer::new(service, handle, CloneParts::new(NoopLayer, acceptor))
    }
}

macro_rules! accept {
    ($handle:expr, $listener:expr) => {
        tokio::select! {
            biased;
            _ = $handle.shutdown_signal() => break Mode::Shutdown,
            _ = $handle.graceful_shutdown_signal() => break Mode::Graceful,
            result = $listener.accept() => result,
        }
    };
}

impl<S, M> HttpServer<S, M>
where
    S: HyperService<Request<hyper::Body>>,
    M: MakeParts + Clone + Send + Sync + 'static,
    M::Layer: Layer<AddExtension<S, SocketAddr>> + Clone + Send + Sync + 'static,
    <M::Layer as Layer<AddExtension<S, SocketAddr>>>::Service: HyperService<Request<hyper::Body>>,
    M::Acceptor: Accept,
{
    pub fn serve_on(&self, listener: TcpListener) -> ListenerTask {
        let server = self.clone();

        tokio::spawn(async move {
            let mut conns = Vec::new();

            let mode = loop {
                let (stream, addr) = accept!(&server.handle, &listener)?;
                let (layer, acceptor) = server.make_parts.make_parts();

                let service = server.service.clone();
                let service = AddExtension::new(service, addr);
                let service = layer.layer(service);

                let conn = tokio::spawn(async move {
                    if let Ok(stream) = acceptor.accept(stream).await {
                        let _ = Http::new()
                            .serve_connection(stream, service)
                            .with_upgrades()
                            .await;
                    }
                });

                conns.push(conn);
            };

            drop(listener);

            match mode {
                Mode::Shutdown => shutdown_conns(conns),
                Mode::Graceful => tokio::select! {
                    biased;
                    _ = server.handle.shutdown_signal() => shutdown_conns(conns),
                    _ = wait_conns(&mut conns) => (),
                },
            }

            Ok(())
        })
    }
}

fn shutdown_conns(conns: Vec<JoinHandle<()>>) {
    for conn in conns {
        conn.abort();
    }
}

async fn wait_conns(conns: &mut Vec<JoinHandle<()>>) {
    for conn in conns {
        let _ = conn.await;
    }
}

#[derive(Clone)]
pub struct NoopAcceptor;

impl<I> Accept<I> for NoopAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Conn = I;
    type Future = Ready<io::Result<Self::Conn>>;

    fn accept(&self, stream: I) -> Self::Future {
        futures_util::future::ready(Ok(stream))
    }
}

#[derive(Clone)]
pub struct NoopLayer;

impl<S> Layer<S> for NoopLayer {
    type Service = S;

    fn layer(&self, layer: S) -> Self::Service {
        layer
    }
}
