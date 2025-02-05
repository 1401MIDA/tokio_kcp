use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use byte_string::ByteStr;
use kcp::{Error as KcpError, KcpResult};
use log::{debug, error, trace};
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::mpsc,
    task::JoinHandle,
    time,
};

use crate::{config::KcpConfig, session::KcpSessionManager, stream::KcpStream};

pub struct KcpListener {
    udp: Arc<UdpSocket>,
    accept_rx: mpsc::Receiver<(KcpStream, SocketAddr)>,
    task_watcher: JoinHandle<()>,
}

impl Drop for KcpListener {
    fn drop(&mut self) {
        self.task_watcher.abort();
    }
}

impl KcpListener {
    pub async fn bind<A: ToSocketAddrs>(config: KcpConfig, addr: A) -> KcpResult<KcpListener> {
        let udp = UdpSocket::bind(addr).await?;
        let udp = Arc::new(udp);
        let server_udp = udp.clone();

        let (accept_tx, accept_rx) = mpsc::channel(1024 /* backlogs */);
        let task_watcher = tokio::spawn(async move {
            let (close_tx, mut close_rx) = mpsc::channel(64);

            let mut sessions = KcpSessionManager::new();
            let mut packet_buffer = [0u8; 65536];
            loop {
                tokio::select! {
                    conv = close_rx.recv() => {
                        let conv = conv.expect("close_tx closed unexpectly");
                        sessions.close_conv(conv);
                        trace!("session conv: {} removed", conv);
                    }

                    recv_res = udp.recv_from(&mut packet_buffer) => {
                        match recv_res {
                            Err(err) => {
                                error!("udp.recv_from failed, error: {}", err);
                                time::sleep(Duration::from_secs(1)).await;
                            }
                            Ok((n, peer_addr)) => {
                                let packet = &mut packet_buffer[..n];

                                log::trace!("received peer: {}, {:?}", peer_addr, ByteStr::new(packet));

                                let mut conv = kcp::get_conv(packet);
                                if conv == 0 {
                                    // Allocate a conv for client.
                                    conv = sessions.alloc_conv();
                                    debug!("allocate {} conv for peer: {}", conv, peer_addr);

                                    kcp::set_conv(packet, conv);
                                }

                                let session = match sessions.get_or_create(&config, conv, &udp, peer_addr, &close_tx) {
                                    Ok((s, created)) => {
                                        if created {
                                            // Created a new session, constructed a new accepted client
                                            let stream = KcpStream::with_session(s.clone());
                                            if let Err(..) = accept_tx.try_send((stream, peer_addr)) {
                                                debug!("failed to create accepted stream due to channel failure");

                                                // remove it from session
                                                sessions.close_conv(conv);
                                                continue;
                                            }
                                        }

                                        s
                                    },
                                    Err(err) => {
                                        error!("failed to create session, error: {}, peer: {}, conv: {}", err, peer_addr, conv);
                                        continue;
                                    }
                                };

                                // let mut kcp = session.kcp_socket().lock().await;
                                // if let Err(err) = kcp.input(packet) {
                                //     error!("kcp.input failed, peer: {}, conv: {}, error: {}, packet: {:?}", peer_addr, conv, err, ByteStr::new(packet));
                                // }
                                session.input(packet).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(KcpListener {
            udp: server_udp,
            accept_rx,
            task_watcher,
        })
    }

    pub async fn accept(&mut self) -> KcpResult<(KcpStream, SocketAddr)> {
        match self.accept_rx.recv().await {
            Some(s) => Ok(s),
            None => Err(KcpError::IoError(io::Error::new(
                ErrorKind::Other,
                "accept channel closed unexpectly",
            ))),
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }
}

#[cfg(test)]
mod test {
    use super::KcpListener;
    use crate::{config::KcpConfig, stream::KcpStream};
    use futures::future;

    #[tokio::test]
    async fn multi_echo() {
        let _ = env_logger::try_init();

        let mut listener = KcpListener::bind(KcpConfig::default(), "127.0.0.1:0").await.unwrap();
        let server_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();

                tokio::spawn(async move {
                    let mut buffer = [0u8; 8192];
                    while let Ok(n) = stream.recv(&mut buffer).await {
                        let data = &buffer[..n];

                        let mut sent = 0;
                        while sent < n {
                            let sn = stream.send(&data[sent..]).await.unwrap();
                            sent += sn;
                        }
                    }
                });
            }
        });

        let mut vfut = Vec::new();

        for _ in 1..100 {
            vfut.push(async move {
                let mut stream = KcpStream::connect(&KcpConfig::default(), server_addr).await.unwrap();

                for _ in 1..20 {
                    const SEND_BUFFER: &[u8] = b"HELLO WORLD";
                    assert_eq!(SEND_BUFFER.len(), stream.send(SEND_BUFFER).await.unwrap());

                    let mut buffer = [0u8; 1024];
                    let n = stream.recv(&mut buffer).await.unwrap();
                    assert_eq!(SEND_BUFFER, &buffer[..n]);
                }
            });
        }

        future::join_all(vfut).await;
    }
}
