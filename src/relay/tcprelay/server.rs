// The MIT License (MIT)

// Copyright (c) 2014 Y. T. CHUNG <zonyitoo@gmail.com>

// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:

// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
// COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
// IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! TcpRelay server that running on the server side

use std::sync::Arc;
use std::io::{self, Read, Write, BufReader, ErrorKind};

use coio::Builder;
use coio::net::{TcpListener, TcpStream, Shutdown};

use config::{Config, ServerConfig};
use relay::{socks5, COROUTINE_STACK_SIZE};
use relay::tcprelay::cached_dns::CachedDns;
use relay::tcprelay::stream::{DecryptedReader, EncryptedWriter};
use crypto::cipher;
use crypto::CryptoMode;

#[derive(Clone)]
pub struct TcpRelayServer {
    config: Config,
}

impl TcpRelayServer {
    pub fn new(c: Config) -> TcpRelayServer {
        if c.server.is_empty() {
            panic!("You have to provide a server configuration");
        }
        TcpRelayServer {
            config: c,
        }
    }

    fn accept_loop(s: ServerConfig) {
        let acceptor = TcpListener::bind(&(&s.addr[..], s.port))
                                    .unwrap_or_else(|err| panic!("Failed to bind: {:?}", err));

        info!("Shadowsocks listening on {}:{}", s.addr, s.port);

        let dnscache_arc = Arc::new(CachedDns::with_capacity(s.dns_cache_capacity));

        let pwd = s.method.bytes_to_key(s.password.as_bytes());
        let timeout = s.timeout;
        let method = s.method;
        for s in acceptor.incoming() {
            let mut stream = match s {
                Ok(s) => s,
                Err(err) => {
                    error!("Error occurs while accepting: {:?}", err);
                    continue;
                }
            };

            if let Err(err) = stream.set_keepalive(timeout) {
                error!("Failed to set keep alive: {:?}", err);
                continue;
            }

            if let Err(err) = stream.set_nodelay(true) {
                error!("Failed to set no delay: {:?}", err);
                continue;
            }

            let pwd = pwd.clone();
            let encrypt_method = method;
            let dnscache = dnscache_arc.clone();

            Builder::new().stack_size(COROUTINE_STACK_SIZE).spawn(move || {
                let remote_iv = {
                    let mut iv = Vec::with_capacity(encrypt_method.block_size());
                    unsafe {
                        iv.set_len(encrypt_method.block_size());
                    }

                    let mut total_len = 0;
                    while total_len < encrypt_method.block_size() {
                        match stream.read(&mut iv[total_len..]) {
                            Ok(0) => {
                                error!("Unexpected EOF while reading initialize vector");
                                return;
                            },
                            Ok(n) => total_len += n,
                            Err(err) => {
                                error!("Error while reading initialize vector: {:?}", err);
                                return;
                            }
                        }
                    }
                    iv
                };
                let decryptor = cipher::with_type(encrypt_method,
                                                  &pwd[..],
                                                  &remote_iv[..],
                                                  CryptoMode::Decrypt);

                let client_reader = match stream.try_clone() {
                    Ok(s) => s,
                    Err(err) => {
                        error!("Error occurs while cloning client stream: {:?}", err);
                        return;
                    }
                };
                let mut client_writer = stream;

                let iv = encrypt_method.gen_init_vec();
                let encryptor = cipher::with_type(encrypt_method,
                                                  &pwd[..],
                                                  &iv[..],
                                                  CryptoMode::Encrypt);
                if let Err(err) = client_writer.write_all(&iv[..]) {
                    error!("Error occurs while writing initialize vector: {:?}", err);
                    return;
                }

                let mut decrypt_stream = DecryptedReader::new(client_reader,
                                                              decryptor);

                let addr = match socks5::Address::read_from(&mut decrypt_stream) {
                    Ok(addr) => addr,
                    Err(err) => {
                        error!("Error occurs while parsing request header, maybe wrong crypto method or password: {}",
                           err);
                        return;
                    }
                };

                info!("Connecting to {}", addr);

                let remote_stream = match &addr {
                    &socks5::Address::SocketAddress(ref addr) => {
                        match TcpStream::connect(&addr) {
                            Ok(stream) => stream,
                            Err(err) => {
                                error!("Unable to connect {:?}: {}", addr, err);
                                return;
                            }
                        }
                    },
                    &socks5::Address::DomainNameAddress(ref dname, ref port) => {
                        let addrs = match dnscache.resolve(&dname) {
                            Some(addrs) => addrs,
                            None => return,
                        };

                        let processing = || {
                            let mut last_err: Option<io::Result<TcpStream>> = None;
                            for addr in addrs.into_iter() {
                                match TcpStream::connect(&(addr.ip(), *port)) {
                                    Ok(stream) => return Ok(stream),
                                    Err(err) => {
                                        error!("Unable to connect {:?}: {}", addr, err);
                                        last_err = Some(Err(err));
                                    }
                                }
                            }

                            last_err.unwrap()
                        };

                        match processing() {
                            Ok(s) => s,
                            Err(_) => return
                        }
                    }
                };

                let mut remote_writer = match remote_stream.try_clone() {
                    Ok(s) => s,
                    Err(err) => {
                        error!("Error occurs while cloning remote stream: {:?}", err);
                        return;
                    }
                };
                let addr_cloned = addr.clone();

                Builder::new().stack_size(COROUTINE_STACK_SIZE).spawn(move|| {
                    let mut remote_reader = BufReader::new(remote_stream);
                    let mut encrypt_stream = EncryptedWriter::new(client_writer, encryptor);
                    match ::relay::copy(&mut remote_reader, &mut encrypt_stream, "Remote to local") {
                        Ok(n) => {
                            let _ = remote_reader.get_ref().peer_addr()
                                .map(|remote_addr| {
                                    encrypt_stream.get_ref().peer_addr()
                                        .map(|client_addr| {
                                            debug!("Remote to local: relayed {} bytes from {} to {}", n,
                                                   remote_addr, client_addr);
                                        })
                                });
                        },
                        Err(err) => {
                            match err.kind() {
                                ErrorKind::BrokenPipe => {
                                    debug!("{} relay from remote to local stream: {}", addr, err)
                                },
                                _ => {
                                    error!("{} relay from remote to local stream: {}", addr, err)
                                }
                            }
                        }
                    }

                    trace!("Remote to local relay is going to be closed");

                    let _ = encrypt_stream.get_mut().shutdown(Shutdown::Both);
                    let _ = remote_reader.get_mut().shutdown(Shutdown::Both);
                });

                Builder::new().stack_size(COROUTINE_STACK_SIZE).spawn(move || {
                    match ::relay::copy(&mut decrypt_stream, &mut remote_writer, "Local to remote") {
                        Ok(n) => {
                            let _ = decrypt_stream.get_ref().peer_addr()
                                .map(|client_addr| {
                                    remote_writer.peer_addr()
                                        .map(|remote_addr| {
                                            debug!("Local to remote: relayed {} bytes from {} to {}", n,
                                                   client_addr, remote_addr);
                                        })
                                });
                        },
                        Err(err) => {
                            match err.kind() {
                                ErrorKind::BrokenPipe => {
                                    debug!("{} relay from local to remote stream: {}", addr_cloned, err)
                                },
                                _ => {
                                    error!("{} relay from local to remote stream: {}", addr_cloned, err)
                                }
                            }
                        }
                    }

                    trace!("Local to remote relay is going to be closed");

                    let _ = remote_writer.shutdown(Shutdown::Both);
                    let _ = decrypt_stream.get_mut().shutdown(Shutdown::Both);
                });
            });
        }
    }
}

impl TcpRelayServer {
    pub fn run(&self) {
        for s in self.config.server.iter() {
            let s = s.clone();
            Builder::new().stack_size(COROUTINE_STACK_SIZE).spawn(move || {
                TcpRelayServer::accept_loop(s);
            });
        }
    }
}
