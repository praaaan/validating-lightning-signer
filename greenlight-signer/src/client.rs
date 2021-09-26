use std::os::unix::io::RawFd;

use nix::sys::socket::{AddressFamily, socketpair, SockFlag, SockType};
use serde::Serialize;

use greenlight_protocol::{msgs, Result};

use crate::connection::UnixConnection;

pub(crate) trait Client {
    fn write<M: msgs::TypedMessage + Serialize>(&mut self, msg: M) -> Result<()>;
    fn read(&mut self) -> Result<msgs::Message>;
    fn id(&self) -> u64;
}

pub(crate) struct UnixClient {
    conn: UnixConnection,
}

impl UnixClient {
    pub(crate) fn new(conn: UnixConnection) -> Self {
        Self {
            conn
        }
    }

    #[must_use = "don't leak the client fd"]
    pub(crate) fn new_client(&mut self) -> UnixClient {
        let (fd_a, fd_b) = socketpair(AddressFamily::Unix, SockType::Stream, None, SockFlag::empty()).unwrap();
        self.conn.send_fd(fd_a);
        UnixClient::new(UnixConnection::new(fd_b))
    }

    pub(crate) fn recv_fd(&mut self) -> core::result::Result<RawFd, ()> {
        self.conn.recv_fd()
    }
}

impl Client for UnixClient {
    fn write<M: msgs::TypedMessage + Serialize>(&mut self, msg: M) -> Result<()> {
        msgs::write(&mut self.conn, msg)?;
        Ok(())
    }

    fn read(&mut self) -> Result<msgs::Message> {
        msgs::read(&mut self.conn)
    }

    fn id(&self) -> u64 {
        self.conn.id()
    }
}
