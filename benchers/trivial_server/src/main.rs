#![feature(test)]

extern crate fuzzy_log;
extern crate mio;
extern crate nix;
extern crate env_logger;
extern crate test;


use mio::deprecated::{EventLoop, Handler as MioHandler, Sender as MioSender};
use mio::tcp::*;

use nix::sys::socket::setsockopt;
use nix::sys::socket::sockopt::TcpNoDelay;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::collections::hash_map::Entry::Vacant;
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use std::{env, mem, ops, thread};
use std::time::Instant;

use fuzzy_log::prelude::*;

use test::black_box;

use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT, Ordering};
use std::{iter};
use std::sync::{Arc, Mutex, mpsc};

//use fuzzy_log::servers::tcp::Server as TcpServer;
use fuzzy_log::servers2;
use fuzzy_log::buffer::Buffer;

use fuzzy_log::async::fuzzy_log::{LogHandle, ThreadLog, Message};
use fuzzy_log::async::store::AsyncTcpStore;

const LISTENER_TOKEN: mio::Token = mio::Token(0);

fn main() {
    let args = parse_args();
    let _ = env_logger::init();

    static SERVER_READY: AtomicUsize = ATOMIC_USIZE_INIT;

    let (addr, num_clients, _h) = match args {
        Args::Server(addr, num_workers) => run_server(addr, num_workers),
        Args::Client(addr, num_clients) => (addr, num_clients, None),
        Args::TrivialServer(addr) => run_trivial_server(addr, &SERVER_READY),
        //Args::TrivialServer(addr) => run_bad_server(addr, 7),
        Args::TrivialClient(addr, num_clients) => run_trivial_client(addr, num_clients),
        Args::LocalTest => {
            let addr = "0.0.0.0:13669".parse().expect("invalid inet address");
            let h = thread::spawn(move || run_trivial_server(addr, &SERVER_READY));
            while SERVER_READY.load(Ordering::Acquire) < 1 {}
            //TODO num clients
            (addr, 1, Some(h))
        }
    };

    static CLIENTS_READY: AtomicUsize = ATOMIC_USIZE_INIT;

    let start = Instant::now();

    let joins: Vec<_> = (0..num_clients).map(|client_num| {
        thread::spawn(move || {
            let mut log_handle = LogHandle::<()>::spawn_tcp_log(addr, iter::once(addr),
                [order::from(5), order::from(6), order::from(7)].into_iter().cloned());

            println!("starting client {}", client_num);
            CLIENTS_READY.fetch_add(1, Ordering::SeqCst);
            while CLIENTS_READY.load(Ordering::SeqCst) < num_clients {}

            log_handle.snapshot(order::from(5));
            while let Some(..) = log_handle.get_next() { }

            let start = Instant::now();
            log_handle.snapshot(order::from(6));
            black_box(log_handle.get_next());
            let first_fetch_latency = start.elapsed();
            let mut fetched_packets: u64 = 1;
            while let Some(..) = log_handle.get_next() {
                fetched_packets += 1;
            }
            let time = start.elapsed();

            log_handle.snapshot(order::from(7));
            while let Some(..) = log_handle.get_next() { }

            let s = time.as_secs() as f64 + (time.subsec_nanos() as f64 * 10.0f64.powi(-9));
            let hz = fetched_packets as f64 / s;
            println!("client {}: time for {} reads {:?}, {}s, {:.3} Hz, ff latency {:?}",
                client_num, fetched_packets, time, s, hz, first_fetch_latency);
            hz
        })
    }).collect();

    println!("All clients started");
    let total_hz: f64 = joins.into_iter().map(|j| j.join().unwrap()).sum();
    let end = start.elapsed();
    println!("total Hz {:.3}", total_hz);
    println!("elapsed time {}s", end.as_secs());

    //std::process::exit(0)
/*
    let to_store_m = Arc::new(Mutex::new(None));
    let tsm = to_store_m.clone();
    let (to_log, from_outside) = mpsc::channel();
    let client = to_log.clone();
    let (ready_reads_s, ready_reads_r) = mpsc::channel();
    let (finished_writes_s, finished_writes_r) = mpsc::channel();
    thread::spawn(move || {
        run_store(addr, client, tsm)
    });
    let to_store;
    loop {
        let ts = mem::replace(&mut *to_store_m.lock().unwrap(), None);
        if let Some(s) = ts {
            to_store = s;
            break
        }
    }
    thread::spawn(move || {
        run_log(to_store, from_outside, ready_reads_s, finished_writes_s)
    });

    let mut log_handle = LogHandle::<()>::new(to_log, ready_reads_r, finished_writes_r);
    log_handle.snapshot(order::from(5));

    let start = Instant::now();
    black_box(log_handle.get_next());
    let first_fetch_latency = start.elapsed();
    for _ in 0..1000000 {
        black_box(log_handle.get_next());
    }
    let time = start.elapsed();
    let s = time.as_secs() as f64 + (time.subsec_nanos() as f64 * 10.0f64.powi(-9));
    let hz = 1000000.0 / s;
    println!("elapsed time for 1000000 reads {:?}, {}s, {:.3} Hz, ff latency {:?}",
        time, s, hz, first_fetch_latency);
    */
}

enum Args {
    LocalTest,
    Server(SocketAddr, usize),
    Client(SocketAddr, usize),
    TrivialServer(SocketAddr),
    TrivialClient(SocketAddr, usize),
}

fn parse_args() -> Args {
    let mut args = env::args().skip(1).take(3);
    let arg0 = args.next();
    let arg1 = args.next();
    match (arg0.as_ref().map(|s| s.as_ref()), arg1.as_ref().map(|s| s.as_ref())) {
        (Some("-c"), Some(addr)) => {
            println!("connecting to remote server @ {}.", addr);
            let num_clients =
                if let Some(n) = args.next() { n.parse().unwrap() }
                else { 1 };
            let addr: &str = addr;
            Args::Client(addr.parse().expect("invalid addr"), num_clients)
        }
        (Some("-s"), Some(port)) => {
            let num_workers =
                if let Some(n) = args.next() { n.parse().unwrap() }
                else { 1 };
            println!("starting server on port {} with {} worker threads.", port, num_workers);
            let mut addr = String::from("0.0.0.0:");
            addr.push_str(port);
            Args::Server(addr.parse().expect("invalid port"), num_workers)
        }
        (Some("-l"), None) => {
            println!("starting local test.");
            Args::LocalTest
        }
        (Some("-ts"), Some(port)) => {
            println!("starting trivial server on port {}.", port);
            let mut addr = String::from("0.0.0.0:");
            addr.push_str(port);
            Args::TrivialServer(addr.parse().expect("invalid port"))
        }
        (Some("-tc"), Some(addr)) => {
            let num_clients =
                if let Some(n) = args.next() { n.parse().unwrap() }
                else { 1 };
            println!("{:?} trivial client(s) connecting to remote server @ {}.", num_clients, addr);
            let addr: &str = addr;
            Args::TrivialClient(addr.parse().expect("invalid addr"), num_clients)
        }
        _ => unimplemented!()
    }
}

#[inline(never)]
pub fn run_trivial_server(addr: SocketAddr, server_ready: &AtomicUsize) -> ! {
    //let mut event_loop = EventLoop::new().unwrap();
    //let server = Server::new(&addr, &mut event_loop);
    //if let Ok(mut server) = server {
    //    server_ready.fetch_add(1, Ordering::Release);
    //    event_loop.run(&mut server).expect("should never return");
    //    panic!("server should never return")
    //}
    //else { panic!("socket in use") }
    Server::run(&addr)
}

#[inline(never)]
pub fn run_server(addr: SocketAddr, num_workers: usize) -> ! {
    // let mut event_loop = EventLoop::new().unwrap();
    // let server = TcpServer::new(&addr, 0, 1, &mut event_loop);
    // if let Ok(mut server) = server {
        // let _ = event_loop.run(&mut server);
        // panic!("server should never return")
    // }
    // else { panic!("socket in use") }
    let acceptor = mio::tcp::TcpListener::bind(&addr);
    if let Ok(acceptor) = acceptor {
        servers2::tcp::run(acceptor, 0, 1, num_workers, &AtomicUsize::new(0))
    }
    else {
        panic!("socket in use")
    }
}

#[inline(never)]
pub fn run_store(
    addr: SocketAddr,
    client: mpsc::Sender<Message>,
    tsm: Arc<Mutex<Option<MioSender<Vec<u8>>>>>
) {
    let mut event_loop = EventLoop::new().unwrap();
    let to_store = event_loop.channel();
    *tsm.lock().unwrap() = Some(to_store);
    let mut store = AsyncTcpStore::tcp(addr,
        iter::once(addr),
        client, &mut event_loop).expect("");
    event_loop.run(&mut store).expect("should never return")
}

#[inline(never)]
pub fn run_log(
    to_store: MioSender<Vec<u8>>,
    from_outside: mpsc::Receiver<Message>,
    ready_reads_s: mpsc::Sender<Vec<u8>>,
    finished_writes_s: mpsc::Sender<(Uuid, Vec<OrderIndex>)>,
) {
    let log = ThreadLog::new(to_store, from_outside, ready_reads_s, finished_writes_s,
        [order::from(5)].into_iter().cloned());
    log.run()
}

fn run_trivial_client(server_addr: SocketAddr, num_clients: usize) -> ! {
    use std::io::{Read, Write};

    let start = Instant::now();
    static CLIENTS_READY: AtomicUsize = ATOMIC_USIZE_INIT;

    let joins: Vec<_> = (0..num_clients).map(|client_num| {
        thread::spawn(move || {
            let stream = Arc::new(std::net::TcpStream::connect(server_addr).unwrap());
            let s1 = stream.clone();
            let _h = thread::spawn(move || {
                let mut stream = &*s1;
                let mut buffer = Vec::new();
                CLIENTS_READY.fetch_add(1, Ordering::SeqCst);
                while CLIENTS_READY.load(Ordering::SeqCst) < num_clients * 2 { thread::yield_now()
                }
                {
                    let e = EntryContents::Data(&(), &[]).fill_vec(&mut buffer);
                    e.kind = EntryKind::Read;
                    e.locs_mut()[0] = (5.into(), 3.into());
                }
                for _ in 0..3000001 {
                    let _ = black_box(stream.write_all(&mut buffer));
                }
            });
            CLIENTS_READY.fetch_add(1, Ordering::SeqCst);
            while CLIENTS_READY.load(Ordering::SeqCst) < num_clients * 2 {
                thread::yield_now()
            }

            let mut stream = &*stream;
            //TODO let mut buffer = vec![0u8; mem::size_of::<Entry<(), DataFlex<()>>>()];
            // since the entry is uninhabited extra data is sent
            let mut buffer = vec![0u8; 40];
            let _ = black_box(stream.read_exact(&mut buffer));
            for _ in 0..1000000 {
                black_box(stream.read_exact(&mut buffer)).unwrap();
            }
            let start = Instant::now();
            for _ in 0..1000000 {
                black_box(stream.read_exact(&mut buffer)).unwrap();
            }
            let time = start.elapsed();
            for _ in 0..1000000 {
                black_box(stream.read_exact(&mut buffer)).unwrap();
            }
            let s = time.as_secs() as f64 + (time.subsec_nanos() as f64 * 10.0f64.powi(-9));
            let hz = 1000000.0 / s;
            println!("client {:?} elapsed time for 1000000 reads {:?}, {}s, {:.3} Hz",
                client_num, time, s, hz);
            hz
        })
    }).collect();

    while CLIENTS_READY.load(Ordering::SeqCst) < num_clients * 2 {
        thread::yield_now()
    }

    println!("All clients started");
    let total_hz: f64 = joins.into_iter().map(|j| j.join().unwrap()).sum();
    let end = start.elapsed();
    println!("total Hz {:.3}", total_hz);
    println!("elapsed time {}s", end.as_secs());

    std::process::exit(0)
}

#[derive(Copy, Clone)]
enum RecvRes {
    Done,
    Error,
    NeedsMore(usize),
}

#[derive(Copy, Clone, Debug)]
#[repr(u8)]
enum Io { Read, Write, ReadWrite }

fn run_bad_server(addr: SocketAddr, num_workers: usize) -> ! {
    use servers2::spmc;
    let acceptor = mio::tcp::TcpListener::bind(&addr).unwrap();

    let (dist_to_workers, recv_from_dist) = spmc::channel();
    let (workers_to_dist, dist_from_workers) = mio::channel::channel();
    for _ in 0..num_workers {
        let from_dist = recv_from_dist.clone();
        let to_dist   = workers_to_dist.clone();
        thread::spawn(move ||
            run_bad_worker(
                from_dist,
                to_dist,
            )
        );
    }

    const ACCEPT: mio::Token = mio::Token(0);
    const FROM_WORKERS: mio::Token = mio::Token(1);
    let poll = mio::Poll::new().unwrap();
    poll.register(&acceptor,
        ACCEPT,
        mio::Ready::readable(),
        mio::PollOpt::level()
    );
    poll.register(&dist_from_workers,
        FROM_WORKERS,
        mio::Ready::readable(),
        mio::PollOpt::level()
    );
    let mut receivers: HashMap<_, _> = Default::default();
    let mut events = mio::Events::with_capacity(1023);
    let mut next_token = mio::Token(2);
    let mut buffer_cache = Vec::new();
    loop {
        poll.poll(&mut events, None).unwrap();
        for event in events.iter() {
            match event.token() {
                ACCEPT => {
                    match acceptor.accept() {
                        Err(e) => {},
                        Ok((socket, addr)) => {
                            let _ = socket.set_keepalive_ms(Some(1000));
                            //TODO benchmark
                            let _ = socket.set_nodelay(true);
                            //TODO oveflow
                            let tok = get_next_token(&mut next_token);
                            poll.register(
                                &socket,
                                tok,
                                mio::Ready::readable(),
                                mio::PollOpt::edge() | mio::PollOpt::oneshot(),
                            );
                            receivers.insert(tok, Some(socket));
                        }
                    }
                }
                FROM_WORKERS => {
                    while let Ok((buffer, socket, tok)) = dist_from_workers.try_recv() {
                        buffer_cache.push(buffer);
                        poll.reregister(
                            &socket,
                            tok,
                            mio::Ready::readable(),
                            mio::PollOpt::edge() | mio::PollOpt::oneshot(),
                        );
                        *receivers.get_mut(&tok).unwrap() = Some(socket)
                    }
                },
                recv_tok => {
                    let recv = receivers.get_mut(&recv_tok).unwrap();
                    let recv = mem::replace(recv, None);
                    match recv {
                        None => {},
                        Some(socket) => {
                            //TODO should be min size ?
                            let buffer =
                                buffer_cache.pop().unwrap_or(Buffer::empty());
                            dist_to_workers.send((buffer, socket, recv_tok))
                        }
                    }
                }
            }
        }
    }
}
fn run_bad_worker(
    from_dist: servers2::spmc::Receiver<(Buffer, TcpStream, mio::Token)>,
    to_dist: mio::channel::Sender<(Buffer, TcpStream, mio::Token)>
) -> ! {
    use std::collections::hash_map;

    const FROM_DIST: mio::Token = mio::Token(0);
    let poll = mio::Poll::new().unwrap();
    let mut current_io: HashMap<_, _> = Default::default();
    poll.register(
        &from_dist,
        FROM_DIST,
        mio::Ready::readable(),
        mio::PollOpt::level());
    let mut events = mio::Events::with_capacity(127);
    loop {
        poll.poll(&mut events, None).expect("worker poll failed");

        'event: for event in events.iter() {
            if let hash_map::Entry::Occupied(mut o) = current_io.entry(event.token()) {
                let next = {
                    let &mut (ref mut buffer, ref stream, ref mut size, tok, ref mut io) =
                        o.get_mut();
                    match *io {
                        Io::Read => match recv_packet(buffer, stream, *size) {
                            RecvRes::Error => (),
                            RecvRes::NeedsMore(read) => {
                                *size = read;
                                continue 'event
                            }
                            RecvRes::Done => {
                                *io = Io::Write;
                                buffer.ensure_capacity(40);
                                let _ = poll.reregister(
                                    stream,
                                    tok,
                                    mio::Ready::writable() | mio::Ready::error(),
                                    mio::PollOpt::edge()
                                );
                            },
                        },
                        Io::Write => match send_packet(buffer, stream, *size) {
                            None => (),
                            Some(sent) => {
                                *size = sent;
                                continue 'event
                            }
                        },
                        Io::ReadWrite => unimplemented!(),
                    }
                };
                let (buffer, stream, _, tok, _) = o.remove();
                poll.deregister(&stream);
                to_dist.send((buffer, stream, tok));
            }
        }

        'recv: loop {
            match from_dist.try_recv() {
                None => break 'recv,
                Some((mut buffer, stream, tok)) => {
                    let continue_read = recv_packet(&mut buffer, &stream, 0);
                    match continue_read {
                        RecvRes::Error => to_dist.send((buffer, stream, tok)).ok().unwrap(),
                            RecvRes::NeedsMore(read) => {
                                let _ = poll.register(
                                    &stream,
                                    tok,
                                    mio::Ready::readable() | mio::Ready::error(),
                                    mio::PollOpt::edge()
                                );
                                current_io.insert(tok, (buffer, stream, read, tok, Io::Read));
                        }
                        RecvRes::Done => {
                            let _ = poll.reregister(
                                &stream,
                                tok,
                                mio::Ready::writable() | mio::Ready::error(),
                                mio::PollOpt::edge()
                            );
                            current_io.insert(tok, (buffer, stream, 0, tok, Io::Write));
                        },
                    }
                },
            }
        }
    }
}

fn send_packet(buffer: &Buffer, mut stream: &TcpStream, sent: usize) -> Option<usize> {
    use std::io::ErrorKind;
    //TODO
    let bytes_to_write = 40;
    //match stream.write(&buffer.entry_slice()[sent..]) {
    match stream.write(&buffer[sent..bytes_to_write]) {
       Ok(i) if (sent + i) < bytes_to_write => Some(sent + i),
       Err(e) => if e.kind() == ErrorKind::WouldBlock { Some(sent) } else { None },
       _ => {
           None
       }
   }
}

fn recv_packet(buffer: &mut Buffer, mut stream: &TcpStream, mut read: usize) -> RecvRes {
    use std::io::ErrorKind;
    let bhs = base_header_size();
    if read < bhs {
        let r = stream.read(&mut buffer[read..bhs])
            .or_else(|e| if e.kind() == ErrorKind::WouldBlock { Ok(read) } else { Err(e) } )
            .ok();
        match r {
            Some(i) => read += i,
            None => return RecvRes::Error,
        }
        if read < bhs {
            return RecvRes::NeedsMore(read)
        }
    }

    let header_size = buffer.entry().header_size();
    assert!(header_size >= base_header_size());
    if read < header_size {
        let r = stream.read(&mut buffer[read..header_size])
            .or_else(|e| if e.kind() == ErrorKind::WouldBlock { Ok(read) } else { Err(e) } )
            .ok();
        match r {
            Some(i) => read += i,
            None => return RecvRes::Error,
        }
        if read < header_size {
            return RecvRes::NeedsMore(read)
        }
    }

    let size = buffer.entry().entry_size();
    if read < size {
        let r = stream.read(&mut buffer[read..size])
            .or_else(|e| if e.kind() == ErrorKind::WouldBlock { Ok(read) } else { Err(e) } )
            .ok();
        match r {
            Some(i) => read += i,
            None => return RecvRes::Error,
        }
        if read < size {
            return RecvRes::NeedsMore(read);
        }
    }
    debug_assert!(buffer.packet_fits());
    // assert!(payload_size >= header_size);
    buffer.entry_mut().kind.insert(EntryKind::ReadSuccess);
    RecvRes::Done
}

fn get_next_token(token: &mut mio::Token) -> mio::Token {
    let next = token.0.wrapping_add(1);
    if next == 0 { *token = mio::Token(2) }
    else { *token = mio::Token(next) };
    *token
}

#[cfg(test)]
mod tests {
    use super::*;

    use test::Bencher;

    #[bench]
    fn bench_read(b: &mut Bencher) {
        let _ = env_logger::init();
        static SERVER_READY: AtomicUsize = ATOMIC_USIZE_INIT;

        let handle = thread::spawn(|| {
            run_trivial_server(&SERVER_READY)
        });

        while SERVER_READY.load(Ordering::Acquire) < 1 {}

        let to_store_m = Arc::new(Mutex::new(None));
        let tsm = to_store_m.clone();
        let (to_log, from_outside) = mpsc::channel();
        let client = to_log.clone();
        let (ready_reads_s, ready_reads_r) = mpsc::channel();
        let (finished_writes_s, finished_writes_r) = mpsc::channel();
        thread::spawn(move || {
            run_store(client, tsm)
        });
        let to_store;
        loop {
            let ts = mem::replace(&mut *to_store_m.lock().unwrap(), None);
            if let Some(s) = ts {
                to_store = s;
                break
            }
        }
        thread::spawn(move || {
            run_log(to_store, from_outside, ready_reads_s, finished_writes_s)
        });

        let mut log_handle = LogHandle::<()>::new(to_log, ready_reads_r, finished_writes_r);
        log_handle.snapshot(order::from(5));
        b.iter(move|| {black_box(log_handle.get_next());})
    }
}


struct Server {
    acceptor: TcpListener,
    clients: HashMap<mio::Token, PerClient>,
    next_token: usize,
}

struct PerClient {
    buffer: Buffer,
    stream: TcpStream,
    is_reading: bool,
    bytes_read: usize,
    bytes_written: usize,
}

impl Server {
    pub fn new(server_addr: &SocketAddr, event_loop: &mut EventLoop<Self>
    ) -> io::Result<Self> {
        let acceptor = try!(TcpListener::bind(server_addr));
        try!(event_loop.register(&acceptor, mio::Token(0), mio::Ready::readable(),
            mio::PollOpt::level()));
        Ok(Server { acceptor: acceptor, clients: HashMap::new(), next_token: 1})
    }

    pub fn run(server_addr: &SocketAddr) -> ! {
        use mio;
        let poll = mio::Poll::new().unwrap();
        let acceptor = TcpListener::bind(server_addr).unwrap();
        let _ = poll.register(&acceptor,
            mio::Token(0),
            mio::Ready::readable(),
            mio::PollOpt::level()).unwrap();
        let mut events = mio::Events::with_capacity(127);
        loop {
            poll.poll(&mut events, None).unwrap();
            for event in events.iter() {
                if let mio::Token(0) = event.token() {
                    match acceptor.accept() {
                        Err(e) => panic!("error {}", e),
                        Ok((socket, addr)) => {
                            thread::spawn(move || {
                                let _ = socket.set_keepalive_ms(Some(1000));
                                let _ = socket.set_nodelay(true);
                                PerClient::new(socket).run();
                            });
                        }
                    }
                }
            }
        }
    }
}

impl MioHandler for Server {
    type Timeout = ();
    type Message = ();

    fn ready(
        &mut self,
        event_loop: &mut EventLoop<Self>,
        token: mio::Token,
        events: mio::Ready
    ) {
        match token {
            LISTENER_TOKEN => {
                assert!(events.is_readable());
                match self.acceptor.accept() {
                    Err(e) => panic!("error {}", e),
                    Ok((socket, addr)) => {
                        let next_client_id = self.next_token;
                        self.next_token += 1;
                        thread::spawn(move || {
                            let _ = socket.set_keepalive_ms(Some(1000));
                            //let _ = setsockopt(socket.as_raw_fd(), TcpNoDelay, &true);
                            let _ = socket.set_nodelay(true);
                            let client_token = mio::Token(next_client_id);
                            let mut per_client = PerClient::new(socket);
                            let mut client_loop = EventLoop::new().unwrap();
                            client_loop.register(&per_client.stream,
                                    client_token,
                                    mio::Ready::readable() | mio::Ready::error(),
                                    mio::PollOpt::edge() | mio::PollOpt::oneshot())
                            .expect("could not register client socket");
                            let _ = client_loop.run(&mut per_client)
                                .expect(" should never halt");
                        });
                        /*let _ = socket.set_keepalive(Some(1));
                        let _ = setsockopt(socket.as_raw_fd(), TcpNoDelay, &true);
                        // let _ = socket.set_tcp_nodelay(true);
                        let client_token = mio::Token(next_client_id);
                        let client_socket = &match self.clients.entry(client_token) {
                            Vacant(v) => v.insert(PerClient::new(socket)),
                            _ => panic!("re-accept client {:?}", client_token),
                        }.stream;
                        //TODO edge or level?
                        event_loop.register(client_socket,
                                client_token,
                                mio::Ready::readable() | mio::Ready::error(),
                                mio::PollOpt::edge() | mio::PollOpt::oneshot())
                        .expect("could not register client socket")*/
                    }
                }
            }
            client_token => {
                if events.is_error() {
                    self.clients.remove(&client_token);
                    return;
                }

                let client = self.clients.get_mut(&client_token).unwrap();
                let finished_read =
                    if client.is_reading && events.is_readable() { client.read_packet() }
                    else { false };

                let (finished_write, needs_write) =
                    if !client.is_reading && events.is_writable() { client.write_packet() }
                    else { (false, !client.is_reading) };

                let next_interest = match (finished_read, finished_write) {
                    (true, true) => mio::Ready::readable(),
                    (true, false) => mio::Ready::writable(),
                    (false, true) => mio::Ready::readable(),
                    (false, false) if needs_write => mio::Ready::writable(),
                    (false, false) => mio::Ready::readable(),
                };
                event_loop.reregister(
                    &client.stream,
                    client_token,
                    next_interest | mio::Ready::error(),
                    mio::PollOpt::edge() | mio::PollOpt::oneshot())
                .expect("could not reregister client socket")
            }
        }
    }
}

impl MioHandler for PerClient {
    type Timeout = ();
    type Message = ();

    fn ready(
        &mut self,
        event_loop: &mut EventLoop<Self>,
        token: mio::Token,
        events: mio::Ready
    ) {
        print!("iter start");
        if events.is_error() {
            panic!("error {:?}", self.stream.take_error())
        }

        let finished_read =
            if self.is_reading && events.is_readable() { self.read_packet() }
            else { false };

        let (finished_write, needs_write) =
            if !self.is_reading && events.is_writable() { self.write_packet() }
            else { (false, !self.is_reading) };

        let next_interest = match (finished_read, finished_write) {
            (true, true) => mio::Ready::readable(),
            (true, false) => mio::Ready::writable(),
            (false, true) => mio::Ready::readable(),
            (false, false) if needs_write => mio::Ready::writable(),
            (false, false) => mio::Ready::readable(),
        };
        event_loop.reregister(
            &self.stream,
            token,
            next_interest | mio::Ready::error(),
            mio::PollOpt::edge() | mio::PollOpt::oneshot())
        .expect("could not reregister client socket")
    }
}

impl PerClient {
    fn new(stream: TcpStream) -> Self {
        PerClient {
            stream: stream,
            buffer: Buffer::new(),
            bytes_read: 0,
            bytes_written: 0,
            is_reading: true,
        }
    }

    fn run(mut self) -> ! {
        use mio;
        let poll = mio::Poll::new().unwrap();
        let _ = poll.register(&self.stream,
            mio::Token(0),
            mio::Ready::readable() | mio::Ready::error(),
            mio::PollOpt::edge() | mio::PollOpt::oneshot()).unwrap();
        let mut events = mio::Events::with_capacity(127);

        loop {
            poll.poll(&mut events, None).unwrap();
            let event = events.get(0).unwrap();
            let events = event.kind();
            let finished_read =
                if self.is_reading && events.is_readable() { self.read_packet() }
                else { false };

            let (finished_write, needs_write) =
                if !self.is_reading && events.is_writable() { self.write_packet() }
                else { (false, !self.is_reading) };

            let next_interest = match (finished_read, finished_write) {
                (true, true) => mio::Ready::readable(),
                (true, false) => mio::Ready::writable(),
                (false, true) => mio::Ready::readable(),
                (false, false) if needs_write => mio::Ready::writable(),
                (false, false) => mio::Ready::readable(),
            };
            poll.reregister(
                &self.stream,
                mio::Token(0),
                next_interest | mio::Ready::error(),
                mio::PollOpt::edge() | mio::PollOpt::oneshot())
            .expect("could not reregister client socket")
        }
    }

    fn read_packet(&mut self) -> bool {
        let size = mem::size_of::<Entry<(), DataFlex<()>>>();
        if self.bytes_read < size {
            let read = self.stream.read(&mut self.buffer[self.bytes_read..size])
                .unwrap();
            self.bytes_read += read;
            if self.bytes_read < size {
                return false;
            }
        }
        if self.buffer.entry().locs()[0].1 < entry::from(::std::u32::MAX) {
            self.buffer.entry_mut().kind.insert(EntryKind::ReadSuccess);
            self.buffer.entry_mut().kind = EntryKind::ReadData;
        }
        else {
            let packet = self.buffer.entry_mut();
            let (old_id, old_loc) =  (packet.id, packet.locs()[0]);
            let chain: order = old_loc.0;
            *packet = EntryContents::Data(&(), &[(chain, entry::from(10000000))]).clone_entry();
            packet.id = old_id;
            packet.kind = EntryKind::NoValue;
            unsafe {
                packet.as_data_entry_mut().flex.loc = old_loc;
            }
        }
        self.is_reading = false;
        self.buffer.ensure_capacity(40);
        true
    }

    fn write_packet(&mut self) -> (bool, bool) {
        //TODO let len = self.buffer.entry().entry_size();
        let len = 40;
        //println!("SERVER writing {:?}", self.buffer.entry().locs()[0]);
        self.bytes_written += self.stream.write(&self.buffer[self.bytes_written..len]).unwrap();
        if self.bytes_written == len {
            //println!("SERVER finished write {:?}", self.buffer.entry().locs()[0]);
            self.bytes_written = 0;
            self.bytes_read = 0;
            self.is_reading = true;
            (true, false)
        }
        else {
            (false, !self.is_reading)
        }
    }
}
