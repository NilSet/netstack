use netutils::getcfg;
use smoltcp;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::mem;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Instant;
use syscall;

use buffer_pool::{Buffer, BufferPool};
use device::NetworkDevice;
use error::{Error, Result};
use self::ip::IpScheme;
use self::udp_socket::UdpScheme;
use self::tcp_socket::TcpScheme;

mod ip;
mod socket;
mod udp_socket;
mod tcp_socket;

type SocketSet = Rc<RefCell<smoltcp::socket::SocketSet<'static, 'static, 'static>>>;

pub struct Smolnetd {
    network_file: Rc<RefCell<File>>,
    time_file: File,

    iface: smoltcp::iface::EthernetInterface<'static, 'static, 'static, NetworkDevice>,
    socket_set: SocketSet,

    startup_time: Instant,

    ip_scheme: IpScheme,
    udp_scheme: UdpScheme,
    tcp_scheme: TcpScheme,

    input_queue: Rc<RefCell<VecDeque<Buffer>>>,
    input_buffer_pool: BufferPool,
}

impl Smolnetd {
    const INGRESS_PACKET_SIZE: usize = 2048;
    const SOCKET_BUFFER_SIZE: usize = 128; //packets
    const CHECK_TIMEOUT_MS: i64 = 10;

    pub fn new(
        network_file: File,
        ip_file: File,
        udp_file: File,
        tcp_file: File,
        time_file: File,
    ) -> Smolnetd {
        let arp_cache = smoltcp::iface::SliceArpCache::new(vec![Default::default(); 8]);
        let hardware_addr = smoltcp::wire::EthernetAddress::from_str(getcfg("mac").unwrap().trim())
            .expect("Can't parse the 'mac' cfg");
        let local_ip = smoltcp::wire::IpAddress::from_str(getcfg("ip").unwrap().trim())
            .expect("Can't parse the 'ip' cfg.");
        let protocol_addrs = [smoltcp::wire::IpCidr::new(local_ip, 24).unwrap()];
        let default_gw = smoltcp::wire::IpAddress::from_str(getcfg("ip_router").unwrap().trim())
            .expect("Can't parse the 'ip_router' cfg.");
        // trace!("mac {:?} ip {}", hardware_addr, protocol_addrs);
        let input_queue = Rc::new(RefCell::new(VecDeque::new()));
        let network_file = Rc::new(RefCell::new(network_file));
        let network_device = NetworkDevice::new(Rc::clone(&network_file), Rc::clone(&input_queue));
        let iface = smoltcp::iface::EthernetInterface::new(
            Box::new(network_device),
            Box::new(arp_cache) as Box<smoltcp::iface::ArpCache>,
            hardware_addr,
            protocol_addrs,
            default_gw,
        );
        let socket_set = Rc::new(RefCell::new(smoltcp::socket::SocketSet::new(vec![])));
        Smolnetd {
            iface,
            socket_set: Rc::clone(&socket_set),
            startup_time: Instant::now(),
            time_file,
            ip_scheme: IpScheme::new(Rc::clone(&socket_set), ip_file),
            udp_scheme: UdpScheme::new(Rc::clone(&socket_set), udp_file),
            tcp_scheme: TcpScheme::new(Rc::clone(&socket_set), tcp_file),
            input_queue,
            network_file,
            input_buffer_pool: BufferPool::new(Self::INGRESS_PACKET_SIZE),
        }
    }

    pub fn on_network_scheme_event(&mut self) -> Result<Option<()>> {
        if self.read_frames()? > 0 {
            self.poll().map(Some)?;
        }
        Ok(None)
    }

    pub fn on_ip_scheme_event(&mut self) -> Result<Option<()>> {
        self.ip_scheme.on_scheme_event()
    }

    pub fn on_udp_scheme_event(&mut self) -> Result<Option<()>> {
        self.udp_scheme.on_scheme_event()
    }

    pub fn on_tcp_scheme_event(&mut self) -> Result<Option<()>> {
        self.tcp_scheme.on_scheme_event()
    }

    pub fn on_time_event(&mut self) -> Result<Option<()>> {
        let mut time = syscall::data::TimeSpec::default();
        if self.time_file.read(&mut time)? < mem::size_of::<syscall::data::TimeSpec>() {
            panic!();
        }
        let mut time_ms = time.tv_sec * 1000i64 + i64::from(time.tv_nsec) / 1_000_000i64;
        time_ms += Smolnetd::CHECK_TIMEOUT_MS;
        time.tv_sec = time_ms / 1000;
        time.tv_nsec = ((time_ms % 1000) * 1_000_000) as i32;
        self.time_file
            .write_all(&time)
            .map_err(|e| Error::from_io_error(e, "Failed to write to time file"))?;

        self.poll().map(Some)?;
        Ok(None)
    }

    fn poll(&mut self) -> Result<()> {
        let timestamp = self.get_timestamp();
        // trace!("Poll {}", timestamp);
        self.iface
            .poll(&mut *self.socket_set.borrow_mut(), timestamp)
            .expect("poll error");
        self.notify_sockets()
    }

    fn read_frames(&mut self) -> Result<usize> {
        let mut total_frames = 0;
        loop {
            let mut buffer = self.input_buffer_pool.get_buffer();
            let count = self.network_file
                .borrow_mut()
                .read(&mut buffer)
                .map_err(|e| {
                    Error::from_io_error(e, "Failed to read from network file")
                })?;
            if count == 0 {
                break;
            }
            trace!("got frame {}", count);
            buffer.resize(count);
            self.input_queue.borrow_mut().push_back(buffer);
            total_frames += 1;
        }
        Ok(total_frames)
    }

    fn get_timestamp(&self) -> u64 {
        let duration = Instant::now().duration_since(self.startup_time);
        (duration.as_secs() * 1000) + u64::from(duration.subsec_nanos() / 1_000_000)
    }

    fn network_fsync(&mut self) -> syscall::Result<usize> {
        use std::os::unix::io::AsRawFd;
        syscall::fsync(self.network_file.borrow_mut().as_raw_fd() as usize)
    }

    fn notify_sockets(&mut self) -> Result<()> {
        self.ip_scheme.notify_sockets()?;
        self.udp_scheme.notify_sockets()?;
        self.tcp_scheme.notify_sockets()
    }
}

fn post_fevent(scheme_file: &mut File, fd: usize, event: usize, data_len: usize) -> Result<()> {
    scheme_file
        .write(&syscall::Packet {
            id: 0,
            pid: 0,
            uid: 0,
            gid: 0,
            a: syscall::number::SYS_FEVENT,
            b: fd,
            c: event,
            d: data_len,
        })
        .map(|_| ())
        .map_err(|e| Error::from_io_error(e, "failed to post fevent"))
}
