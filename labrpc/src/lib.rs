#![allow(unused_attributes)]
#![allow(clippy::new_without_default)]
#![allow(clippy::if_same_then_else)]
#![feature(test)]
#[cfg(test)]
extern crate test;

extern crate bytes;
#[macro_use]
extern crate log;
#[macro_use]
extern crate futures;
extern crate futures_cpupool;
extern crate futures_timer;
extern crate hashbrown;
extern crate labcodec;
extern crate prost;
extern crate rand;

#[cfg(test)]
#[macro_use]
extern crate prost_derive;
#[cfg(test)]
extern crate env_logger;
#[cfg(test)]
#[macro_use]
extern crate lazy_static;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, ATOMIC_USIZE_INIT};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::{fmt, time};

use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{Async, Future, Poll, Stream};
use futures_cpupool::CpuPool;
use futures_timer::Delay;
use hashbrown::HashMap;
use rand::Rng;

mod error;
#[macro_use]
mod macros;

pub use error::{Error, Result};

static ID_ALLOC: AtomicUsize = ATOMIC_USIZE_INIT;

type Handler = Fn(&[u8], &mut Vec<u8>) -> Result<()> + Send + Sync + 'static;

pub struct ServerBuilder {
    name: String,
    services: HashMap<&'static str, Box<Handler>>,
}

impl ServerBuilder {
    pub fn new(name: String) -> ServerBuilder {
        ServerBuilder {
            name,
            services: HashMap::new(),
        }
    }

    pub fn add_handler(&mut self, fq_name: &'static str, handler: Box<Handler>) -> Result<()> {
        self.services.insert(fq_name, handler);
        Ok(())
    }

    pub fn build(self) -> Server {
        Server {
            core: Arc::new(ServerCore {
                name: self.name,
                services: self.services,
                id: ID_ALLOC.fetch_add(1, Ordering::Relaxed),
                count: AtomicUsize::new(0),
            }),
        }
    }
}

struct ServerCore {
    name: String,
    id: usize,

    services: HashMap<&'static str, Box<Handler>>,
    count: AtomicUsize,
}

#[derive(Clone)]
pub struct Server {
    core: Arc<ServerCore>,
}

impl Server {
    pub fn count(&self) -> usize {
        self.core.count.load(Ordering::SeqCst)
    }

    pub fn name(&self) -> &str {
        &self.core.name
    }

    fn dispatch(&self, fq_name: &str, req: &[u8], rsp: &mut Vec<u8>) -> Result<()> {
        self.core.count.fetch_add(1, Ordering::SeqCst);
        if let Some(handle) = self.core.services.get(fq_name) {
            handle(req, rsp)
        } else {
            Err(Error::Unimplemented(format!("unknown {}", fq_name)))
        }
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Server")
            .field("name", &self.core.name)
            .field("id", &self.core.id)
            .finish()
    }
}

pub struct Rpc {
    end_name: String,
    fq_name: &'static str,
    req: Vec<u8>,
    resp: SyncSender<Result<Vec<u8>>>,
}

impl fmt::Debug for Rpc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Rpc")
            .field("end_name", &self.end_name)
            .field("fq_name", &self.fq_name)
            .finish()
    }
}

#[derive(Clone)]
pub struct ClientEnd {
    // this end-point's name
    end_name: String,
    // copy of Network.sender
    sender: UnboundedSender<Rpc>,
}

impl ClientEnd {
    pub fn call<Req, Rsp>(&self, fq_name: &'static str, req: &Req) -> Result<Rsp>
    where
        Req: prost::Message,
        Rsp: prost::Message + Default,
    {
        let mut buf = vec![];
        labcodec::encode(req, &mut buf).map_err(Error::Encode)?;

        let (tx, rx) = sync_channel(1);
        let rpc = Rpc {
            end_name: self.end_name.clone(),
            fq_name,
            req: buf,
            resp: tx,
        };

        // Sends requets and waits responses.
        self.sender
            .unbounded_send(rpc)
            .map_err(|_| Error::Stopped)?;
        match rx.recv().map_err(Error::Recv) {
            Ok(Ok(resp)) => labcodec::decode(&resp).map_err(Error::Decode),
            Ok(Err(e)) | Err(e) => Err(e),
        }
    }
}

#[derive(Debug)]
struct EndInfo {
    enabled: bool,
    reliable: bool,
    long_reordering: bool,
    server: Option<Server>,
}

struct Endpoints {
    // ends, by name
    // ends: HashMap<String, ClientEnd>,
    // by end name
    enabled: HashMap<String, bool>,
    // servers, by name
    servers: HashMap<String, Option<Server>>,
    // end_name -> server_name
    connections: HashMap<String, Option<String>>,
}

struct Core {
    reliable: AtomicBool,
    // pause a long time on send on disabled connection
    long_delays: AtomicBool,
    // sometimes delay replies a long time
    long_reordering: AtomicBool,
    endpoints: Mutex<Endpoints>,
    count: AtomicUsize,
    sender: UnboundedSender<Rpc>,
    pool: CpuPool,
}

#[derive(Clone)]
pub struct Network {
    core: Arc<Core>,
}

impl Network {
    pub fn new() -> Network {
        let (rn, incoming) = Network::create();
        rn.start(incoming);
        rn
    }

    fn create() -> (Network, UnboundedReceiver<Rpc>) {
        let (sender, incoming) = unbounded();
        let net = Network {
            core: Arc::new(Core {
                reliable: AtomicBool::new(true),
                long_delays: AtomicBool::new(false),
                long_reordering: AtomicBool::new(false),
                endpoints: Mutex::new(Endpoints {
                    enabled: HashMap::new(),
                    servers: HashMap::new(),
                    connections: HashMap::new(),
                }),
                count: AtomicUsize::new(0),
                pool: CpuPool::new_num_cpus(),
                sender,
            }),
        };

        (net, incoming)
    }

    fn start(&self, incoming: UnboundedReceiver<Rpc>) {
        let net = self.clone();
        self.core
            .pool
            .spawn(incoming.for_each(move |rpc| {
                let fut = net.process_rpc(rpc);
                net.core.pool.spawn(fut).forget();
                Ok(())
            }))
            .forget();
    }

    pub fn add_server(&self, server: Server) {
        let mut eps = self.core.endpoints.lock().unwrap();
        eps.servers.insert(server.core.name.clone(), Some(server));
    }

    pub fn delete_server(&self, name: String) {
        let mut eps = self.core.endpoints.lock().unwrap();
        eps.servers.insert(name, None);
    }

    pub fn create_end(&self, end_name: String) -> ClientEnd {
        let sender = self.core.sender.clone();
        let mut eps = self.core.endpoints.lock().unwrap();
        eps.enabled.insert(end_name.clone(), false);
        eps.connections.insert(end_name.clone(), None);
        ClientEnd { end_name, sender }
    }

    /// Connects a ClientEnd to a server.
    /// a ClientEnd can only be connected once in its lifetime.
    pub fn connect(&self, end_name: String, server_name: String) {
        let mut eps = self.core.endpoints.lock().unwrap();
        eps.connections.insert(end_name, Some(server_name));
    }

    /// Enable/disable a ClientEnd.
    pub fn enable(&self, end_name: String, enabled: bool) {
        debug!(
            "client {} is {}",
            end_name,
            if enabled { "enabled" } else { "disbaled" }
        );
        let mut eps = self.core.endpoints.lock().unwrap();
        eps.enabled.insert(end_name, enabled);
    }

    pub fn set_reliable(&self, yes: bool) {
        self.core.reliable.store(yes, Ordering::SeqCst);
    }

    pub fn set_long_reordering(&self, yes: bool) {
        self.core.long_reordering.store(yes, Ordering::SeqCst);
    }

    pub fn set_long_delays(&self, yes: bool) {
        self.core.long_delays.store(yes, Ordering::SeqCst);
    }

    pub fn count(&self, server_name: &str) -> usize {
        let eps = self.core.endpoints.lock().unwrap();
        eps.servers[server_name].as_ref().unwrap().count()
    }

    pub fn total_count(&self) -> usize {
        self.core.count.load(Ordering::SeqCst)
    }

    fn end_info(&self, end_name: &str) -> EndInfo {
        let eps = self.core.endpoints.lock().unwrap();
        let mut server = None;
        if let Some(Some(server_name)) = eps.connections.get(end_name) {
            server = eps.servers[server_name].clone();
        }
        EndInfo {
            enabled: eps.enabled[end_name],
            reliable: self.core.reliable.load(Ordering::SeqCst),
            long_reordering: self.core.long_reordering.load(Ordering::SeqCst),
            server,
        }
    }

    fn is_server_dead(&self, end_name: &str, server_name: &str, server_id: usize) -> bool {
        let eps = self.core.endpoints.lock().unwrap();
        !eps.enabled[end_name]
            || eps.servers.get(server_name).map_or(true, |o| {
                o.as_ref().map(|s| s.core.id != server_id).unwrap_or(true)
            })
    }

    fn process_rpc(&self, rpc: Rpc) -> ProcessRpc {
        self.core.count.fetch_add(1, Ordering::SeqCst);
        let mut random = rand::thread_rng();
        let network = self.clone();
        let end_info = self.end_info(&rpc.end_name);
        debug!("{:?} process with {:?}", rpc, end_info);
        let EndInfo {
            enabled,
            reliable,
            long_reordering,
            server,
        } = end_info;

        if enabled && server.is_some() {
            let server = server.unwrap();
            let short_delay = if !reliable {
                // short delay
                let ms = random.gen::<u64>() % 27;
                Some(Delay::new(time::Duration::from_millis(ms)))
            } else {
                None
            };

            if !reliable && (random.gen::<u64>() % 1000) < 100 {
                // drop the request, return as if timeout
                return ProcessRpc {
                    state: Some(ProcessState::Timeout {
                        delay: short_delay.unwrap(),
                    }),
                    rpc,
                    network,
                };
            }

            // execute the request (call the RPC handler).
            // in a separate thread so that we can periodically check
            // if the server has been killed and the RPC should get a
            // failure reply.

            // do not reply if DeleteServer() has been called, i.e.
            // the server has been killed. this is needed to avoid
            // situation in which a client gets a positive reply
            // to an Append, but the server persisted the update
            // into the old Persister. config.go is careful to call
            // DeleteServer() before superseding the Persister.

            let drop_reply = !reliable && random.gen::<u64>() % 1000 < 100;
            let long_reordering = if long_reordering && random.gen_range(0, 900) < 600i32 {
                // delay the response for a while
                let upper_bound: u64 = 1 + random.gen_range(0, 2000);
                Some(200 + random.gen_range(0, upper_bound))
            } else {
                None
            };
            ProcessRpc {
                state: Some(ProcessState::Dispatch {
                    delay: short_delay,
                    server,
                    drop_reply,
                    long_reordering,
                }),
                rpc,
                network,
            }
        } else {
            // simulate no reply and eventual timeout.
            let ms = if self.core.long_delays.load(Ordering::SeqCst) {
                // let Raft tests check that leader doesn't send
                // RPCs synchronously.
                random.gen::<u64>() % 7000
            } else {
                // many kv tests require the client to try each
                // server in fairly rapid succession.
                random.gen::<u64>() % 100
            };

            debug!("{:?} delay {}ms then timeout", rpc, ms);
            let delay = Delay::new(time::Duration::from_millis(ms));
            ProcessRpc {
                state: Some(ProcessState::Timeout { delay }),
                rpc,
                network,
            }
        }
    }
}

struct ProcessRpc {
    state: Option<ProcessState>,

    rpc: Rpc,
    network: Network,
}

impl fmt::Debug for ProcessRpc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ProcessRpc")
            .field("rpc", &self.rpc)
            .field("state", &self.state)
            .finish()
    }
}

enum ProcessState {
    Timeout {
        delay: Delay,
    },
    Dispatch {
        delay: Option<Delay>,
        server: Server,
        drop_reply: bool,
        long_reordering: Option<u64>,
    },
    Reordering {
        delay: Delay,
        resp: Option<Vec<u8>>,
    },
}

impl fmt::Debug for ProcessState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ProcessState::Timeout { .. } => write!(f, "ProcessState::Timeout"),
            ProcessState::Dispatch {
                ref delay,
                drop_reply,
                long_reordering,
                ..
            } => f
                .debug_struct("ProcessState::Dispatch")
                .field("delay", &delay.is_some())
                .field("drop_reply", &drop_reply)
                .field("long_reordering", &long_reordering)
                .finish(),
            ProcessState::Reordering { .. } => write!(f, "ProcessState::Reordering"),
        }
    }
}

impl Future for ProcessRpc {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            let mut next = None;
            debug!("polling {:?}", self);
            match self
                .state
                .as_mut()
                .expect("cannot poll ProcessRpc after finish")
            {
                ProcessState::Timeout { ref mut delay } => {
                    try_ready!(delay.poll().map_err(|_| ()));
                    self.rpc.resp.send(Err(Error::Timeout)).unwrap();
                }
                ProcessState::Dispatch {
                    ref mut delay,
                    ref server,
                    drop_reply,
                    long_reordering,
                } => {
                    if let Some(ref mut delay) = *delay {
                        try_ready!(delay.poll().map_err(|_| ()));
                    }
                    // We has finished the delay, take it out to prevent polling
                    // twice.
                    delay.take();
                    // TODO: execute the request (call the RPC handler).
                    // in a separate thread so that we can periodically check
                    // if the server has been killed and the RPC should get a
                    // failure reply.
                    let mut buf = vec![];
                    let res = server.dispatch(self.rpc.fq_name, &self.rpc.req, &mut buf);
                    if let Err(e) = res {
                        self.rpc.resp.send(Err(e)).unwrap();
                    } else if self.network.is_server_dead(
                        &self.rpc.end_name,
                        &server.core.name,
                        server.core.id,
                    ) {
                        // server was killed while we were waiting; return error,
                        self.rpc.resp.send(Err(Error::Timeout)).unwrap();
                    } else if *drop_reply {
                        //  drop the reply, return as if timeout.
                        self.rpc.resp.send(Err(Error::Timeout)).unwrap();
                    } else if let Some(reordering) = long_reordering {
                        debug!("{:?} next long reordering {}ms", self.rpc, reordering);
                        next = Some(ProcessState::Reordering {
                            delay: Delay::new(time::Duration::from_millis(*reordering)),
                            resp: Some(buf),
                        });
                    } else {
                        self.rpc.resp.send(Ok(buf)).unwrap();
                    }
                }
                ProcessState::Reordering {
                    ref mut delay,
                    ref mut resp,
                } => {
                    try_ready!(delay.poll().map_err(|_| ()));
                    self.rpc.resp.send(Ok(resp.take().unwrap())).unwrap();
                }
            }
            if let Some(next) = next {
                self.state = Some(next);
            } else {
                self.state.take();
                return Ok(Async::Ready(()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::RecvError;
    use std::sync::{mpsc, Mutex};
    use std::thread;

    use test;

    use super::*;

    service! {
        /// A simple test-purpose service.
        service junk {
            /// Doc comments.
            rpc handler2(JunkArgs) returns JunkReply;
            rpc handler4(JunkArgs) returns JunkReply;
        }
    }
    use tests::junk::{add_service, Client as JunkClient, Service as JunkService};

    // Hand-written protobuf messages.
    #[derive(Clone, PartialEq, Message)]
    pub struct JunkArgs {
        #[prost(int64, tag = "1")]
        pub x: i64,
    }
    #[derive(Clone, PartialEq, Message)]
    pub struct JunkReply {
        #[prost(string, tag = "1")]
        pub x: String,
    }

    #[derive(Default)]
    struct JunkInner {
        log2: Vec<i64>,
    }
    #[derive(Clone)]
    struct JunkServer {
        inner: Arc<Mutex<JunkInner>>,
    }
    impl JunkServer {
        fn new() -> JunkServer {
            JunkServer {
                inner: Arc::default(),
            }
        }
    }
    impl JunkService for JunkServer {
        fn handler2(&self, args: JunkArgs) -> JunkReply {
            self.inner.lock().unwrap().log2.push(args.x);
            JunkReply {
                x: format!("handler2-{}", args.x),
            }
        }
        fn handler4(&self, _: JunkArgs) -> JunkReply {
            JunkReply {
                x: "pointer".to_owned(),
            }
        }
    }

    lazy_static! {
        static ref LOGGER_INIT: () = env_logger::init();
    }

    #[test]
    fn test_service_dispatch() {
        *LOGGER_INIT;

        let mut builder = ServerBuilder::new("test".to_owned());
        let junk = JunkServer::new();
        add_service(&junk, &mut builder).unwrap();
        let server = builder.build();

        let mut buf = Vec::new();
        server.dispatch("junk.handler4", &[], &mut buf).unwrap();
        let rsp = labcodec::decode(&buf).unwrap();
        assert_eq!(
            JunkReply {
                x: "pointer".to_owned(),
            },
            rsp,
        );

        buf.clear();
        server
            .dispatch("junk.handler4", b"bad message", &mut buf)
            .unwrap_err();
        assert!(buf.is_empty());

        buf.clear();
        server
            .dispatch("badjunk.handler4", &[], &mut buf)
            .unwrap_err();
        assert!(buf.is_empty());

        buf.clear();
        server
            .dispatch("junk.badhandler", &[], &mut buf)
            .unwrap_err();
        assert!(buf.is_empty());
    }

    #[test]
    fn test_network_client_rpc() {
        *LOGGER_INIT;

        let mut builder = ServerBuilder::new("test".to_owned());
        let junk = JunkServer::new();
        add_service(&junk, &mut builder).unwrap();
        let server = builder.build();

        let (rn, incoming) = Network::create();
        rn.add_server(server);

        let client = JunkClient::new(rn.create_end("test_client".to_owned()));
        let client_ = client.clone();
        let handler = thread::spawn(move || client_.handler4(&JunkArgs { x: 777 }));
        let (rpc, incoming) = match incoming.into_future().wait() {
            Ok((Some(rpc), s)) => (rpc, s),
            _ => panic!("unexpected error"),
        };
        let reply = JunkReply {
            x: "boom!!!".to_owned(),
        };
        let mut buf = vec![];
        labcodec::encode(&reply, &mut buf).unwrap();
        rpc.resp.send(Ok(buf)).unwrap();
        assert_eq!(rpc.end_name, "test_client");
        assert_eq!(rpc.fq_name, "junk.handler4");
        assert!(!rpc.req.is_empty());
        assert_eq!(handler.join().unwrap(), Ok(reply));

        let client_ = client.clone();
        let handler = thread::spawn(move || client_.handler4(&JunkArgs { x: 777 }));
        let (rpc, incoming) = match incoming.into_future().wait() {
            Ok((Some(rpc), s)) => (rpc, s),
            _ => panic!("unexpected error"),
        };
        drop(rpc.resp);
        assert_eq!(handler.join().unwrap(), Err(Error::Recv(RecvError)));

        drop(incoming);
        assert_eq!(client.handler4(&JunkArgs::default()), Err(Error::Stopped));
    }

    #[test]
    fn test_basic() {
        *LOGGER_INIT;

        let (rn, _server, _) = junk_suit();

        let client = JunkClient::new(rn.create_end("test_client".to_owned()));
        rn.connect("test_client".to_owned(), "test_server".to_owned());
        rn.enable("test_client".to_owned(), true);

        let rsp = client.handler4(&JunkArgs::default()).unwrap();
        assert_eq!(
            JunkReply {
                x: "pointer".to_owned(),
            },
            rsp,
        );
    }

    // does net.Enable(endname, false) really disconnect a client?
    #[test]
    fn test_disconnect() {
        // *LOGGER_INIT;

        let (rn, _server, _) = junk_suit();

        let client = JunkClient::new(rn.create_end("test_client".to_owned()));
        rn.connect("test_client".to_owned(), "test_server".to_owned());

        client.handler4(&JunkArgs::default()).unwrap_err();

        rn.enable("test_client".to_owned(), true);
        let rsp = client.handler4(&JunkArgs::default()).unwrap();

        assert_eq!(
            JunkReply {
                x: "pointer".to_owned(),
            },
            rsp,
        );
    }

    // test net.GetCount()
    #[test]
    fn test_count() {
        // *LOGGER_INIT;

        let (rn, _server, _) = junk_suit();

        let client = JunkClient::new(rn.create_end("test_client".to_owned()));
        rn.connect("test_client".to_owned(), "test_server".to_owned());
        rn.enable("test_client".to_owned(), true);

        for i in 0..=16 {
            let reply = client.handler2(&JunkArgs { x: i }).unwrap();
            assert_eq!(reply.x, format!("handler2-{}", i));
        }

        assert_eq!(rn.count("test_server"), 17,);
    }

    // test RPCs from concurrent ClientEnds
    #[test]
    fn test_concurrent_many() {
        *LOGGER_INIT;

        let (rn, server, _) = junk_suit();
        let server_name = server.name();

        let pool = futures_cpupool::CpuPool::new_num_cpus();
        let (tx, rx) = mpsc::channel::<usize>();

        let nclients = 20usize;
        let nrpcs = 10usize;
        for i in 0..nclients {
            let net = rn.clone();
            let sender = tx.clone();
            let server_name_ = server_name.to_string();

            pool.spawn_fn(move || {
                let mut n = 0;
                let client_name = format!("client-{}", i);
                let client = JunkClient::new(net.create_end(client_name.clone()));
                net.enable(client_name.clone(), true);
                net.connect(client_name.clone(), server_name_);

                for j in 0..nrpcs {
                    let x = (i * 100 + j) as i64;
                    let reply = client.handler2(&JunkArgs { x }).unwrap();
                    assert_eq!(reply.x, format!("handler2-{}", x));
                    n += 1;
                }

                sender.send(n)
            })
            .forget();
        }

        let mut total = 0;
        for _ in 0..nclients {
            total += rx.recv().unwrap();
        }
        assert_eq!(total, nrpcs * nclients);
        let n = rn.count(&server_name);
        assert_eq!(n, total);
    }

    fn junk_suit() -> (Network, Server, JunkServer) {
        let rn = Network::new();
        let server_name = "test_server".to_owned();
        let mut builder = ServerBuilder::new(server_name.clone());
        let junk_server = JunkServer::new();
        add_service(&junk_server, &mut builder).unwrap();
        let server = builder.build();
        rn.add_server(server.clone());
        (rn, server, junk_server)
    }

    #[test]
    fn test_unreliable() {
        *LOGGER_INIT;

        let (rn, server, _) = junk_suit();
        let server_name = server.name();
        rn.set_reliable(false);

        let pool = futures_cpupool::CpuPool::new_num_cpus();
        let (tx, rx) = mpsc::channel::<usize>();
        let nclients = 300;
        for i in 0..nclients {
            let sender = tx.clone();
            let mut n = 0;
            let server_name_ = server_name.to_owned();
            let net = rn.clone();

            pool.spawn_fn(move || {
                let client_name = format!("client-{}", i);
                let client = JunkClient::new(net.create_end(client_name.clone()));
                net.enable(client_name.clone(), true);
                net.connect(client_name.clone(), server_name_);

                let x = i * 100;
                if let Ok(reply) = client.handler2(&JunkArgs { x }) {
                    assert_eq!(reply.x, format!("handler2-{}", x));
                    n += 1;
                }
                sender.send(n)
            })
            .forget();
        }
        let mut total = 0;
        for _ in 0..nclients {
            total += rx.recv().unwrap();
        }
        assert!(
            !(total == nclients as _ || total == 0),
            "all RPCs succeeded despite unreliable total {}, nclients {}",
            total,
            nclients
        );
    }

    // test concurrent RPCs from a single ClientEnd
    #[test]
    fn test_concurrent_one() {
        *LOGGER_INIT;

        let (rn, server, junk_server) = junk_suit();
        let server_name = server.name();

        let pool = futures_cpupool::CpuPool::new_num_cpus();
        let (tx, rx) = mpsc::channel::<usize>();
        let nrpcs = 20;
        for i in 0..20 {
            let sender = tx.clone();
            let client_name = format!("client-{}", i);
            let client = JunkClient::new(rn.create_end(client_name.clone()));
            rn.enable(client_name.clone(), true);
            rn.connect(client_name.clone(), server_name.to_owned());

            pool.spawn_fn(move || {
                let mut n = 0;
                let x = i + 100;
                let reply = client.handler2(&JunkArgs { x }).unwrap();
                assert_eq!(reply.x, format!("handler2-{}", x));
                n += 1;
                sender.send(n)
            })
            .forget();
        }

        let mut total = 0;
        for _ in 0..nrpcs {
            total += rx.recv().unwrap();
        }
        assert!(
            total == nrpcs,
            "wrong number of RPCs completed, got {}, expected {}",
            total,
            nrpcs
        );

        assert_eq!(
            junk_server.inner.lock().unwrap().log2.len(),
            nrpcs,
            "wrong number of RPCs delivered"
        );

        let n = rn.count(server.name());
        assert_eq!(n, total, "wrong count() {}, expected {}", n, total);
    }

    // regression: an RPC that's delayed during Enabled=false
    // should not delay subsequent RPCs (e.g. after Enabled=true).
    #[test]
    fn test_regression1() {
        *LOGGER_INIT;

        let (rn, server, junk_server) = junk_suit();
        let server_name = server.name();

        let client_name = format!("client");
        let client = JunkClient::new(rn.create_end(client_name.clone()));
        rn.connect(client_name.clone(), server_name.to_owned());

        // start some RPCs while the ClientEnd is disabled.
        // they'll be delayed.
        rn.enable(client_name.clone(), false);

        let pool = futures_cpupool::CpuPool::new_num_cpus();
        let (tx, rx) = mpsc::channel::<bool>();
        let nrpcs = 20;
        for i in 0..20 {
            let sender = tx.clone();
            let client_ = client.clone();
            pool.spawn_fn(move || {
                let x = i + 100;
                // this call ought to return false.
                let _ = client_.handler2(&JunkArgs { x });
                sender.send(true)
            })
            .forget();
        }

        // FIXME: have to sleep 300ms to pass the test
        //        in my computer (i5-4200U, 4 threads).
        thread::sleep(time::Duration::from_millis(100 * 3));

        let t0 = time::Instant::now();
        rn.enable(client_name.clone(), true);
        let x = 99;
        let reply = client.handler2(&JunkArgs { x }).unwrap();
        assert_eq!(reply.x, format!("handler2-{}", x));
        let dur = t0.elapsed();
        assert!(
            dur < time::Duration::from_millis(30),
            "RPC took too long ({:?}) after Enable",
            dur
        );

        for _ in 0..nrpcs {
            rx.recv().unwrap();
        }

        let len = junk_server.inner.lock().unwrap().log2.len();
        assert!(
            len == 1,
            "wrong number ({}) of RPCs delivered, expected 1",
            len
        );

        let n = rn.count(server.name());
        assert!(n == 1, "wrong count() {}, expected 1", n);
    }

    // if an RPC is stuck in a server, and the server
    // is killed with DeleteServer(), does the RPC
    // get un-stuck?
    // #[test]
    // fn test_killed() {
    //     unimplemented!();
    // }

    #[bench]
    fn bench_rpc(b: &mut test::Bencher) {
        let (rn, server, _junk_server) = junk_suit();
        let server_name = server.name();
        let client_name = format!("client");
        let client = JunkClient::new(rn.create_end(client_name.clone()));
        rn.connect(client_name.clone(), server_name.to_owned());
        rn.enable(client_name.clone(), true);

        b.iter(|| {
            client.handler2(&JunkArgs { x: 111 }).unwrap();
        });
        // i5-4200U, 19.4 microseconds per RPC
    }
}
