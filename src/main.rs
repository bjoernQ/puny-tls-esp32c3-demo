#![no_std]
#![no_main]

use embedded_hal::watchdog::WatchdogDisable;
use embedded_io::blocking::{Read, Write};
use embedded_io::Io;
use embedded_svc::wifi::{
    ClientConfiguration, ClientConnectionStatus, ClientIpStatus, ClientStatus, Configuration,
    Status, Wifi,
};
use esp32c3_hal::clock::{ClockControl, CpuClock};
use esp32c3_hal::prelude::*;
use esp32c3_hal::Rtc;
use esp_println::println;
use esp_wifi::wifi::initialize;
use esp_wifi::wifi::utils::create_network_interface;
use esp_wifi::wifi_interface::timestamp;
use esp_wifi::{create_network_stack_storage, network_stack_storage};

use esp32c3_hal::pac::Peripherals;
use esp_backtrace as _;
use puny_tls::Session;
use rand_core::RngCore;
use riscv_rt::entry;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::TcpSocket;
use smoltcp::time::Instant;
use smoltcp::wire::Ipv4Address;

extern crate alloc;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

#[entry]
fn main() -> ! {
    // init_logger();
    esp_wifi::init_heap();

    let mut peripherals = Peripherals::take().unwrap();
    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock160MHz).freeze();

    let mut rtc_cntl = Rtc::new(peripherals.RTC_CNTL);

    // Disable watchdog timers
    rtc_cntl.swd.disable();
    rtc_cntl.rwdt.disable();

    let mut storage = create_network_stack_storage!(3, 8, 1);
    let ethernet = create_network_interface(network_stack_storage!(storage));
    let mut wifi_interface = esp_wifi::wifi_interface::Wifi::new(ethernet);

    initialize(&mut peripherals.SYSTIMER, peripherals.RNG, &clocks).unwrap();

    println!("{:?}", wifi_interface.get_status());

    println!("Call wifi_connect");
    let client_config = Configuration::Client(ClientConfiguration {
        ssid: SSID.into(),
        password: PASSWORD.into(),
        ..Default::default()
    });
    let res = wifi_interface.set_configuration(&client_config);
    println!("wifi_connect returned {:?}", res);

    println!("{:?}", wifi_interface.get_capabilities());
    println!("{:?}", wifi_interface.get_status());

    // wait to get connected
    loop {
        if let Status(ClientStatus::Started(_), _) = wifi_interface.get_status() {
            break;
        }
    }
    println!("{:?}", wifi_interface.get_status());

    // wait to get connected and have an ip
    loop {
        wifi_interface.poll_dhcp().unwrap();

        wifi_interface
            .network_interface()
            .poll(timestamp())
            .unwrap();

        if let Status(
            ClientStatus::Started(ClientConnectionStatus::Connected(ClientIpStatus::Done(config))),
            _,
        ) = wifi_interface.get_status()
        {
            println!("got ip {:?}", config);
            break;
        }
    }

    println!("We are connected!");

    let (http_socket_handle, _) = wifi_interface
        .network_interface()
        .sockets_mut()
        .next()
        .unwrap();
    let (socket, cx) = wifi_interface
        .network_interface()
        .get_socket_and_context::<TcpSocket>(http_socket_handle);

    let remote_endpoint = (Ipv4Address::new(23, 15, 178, 162), 443); // tls13.akamai.io
    socket.connect(cx, remote_endpoint, 41000).unwrap();

    let io = InputOutput::new(wifi_interface, http_socket_handle, current_millis);
    let mut rng = Rng::new();

    let mut tls: puny_tls::Session<'_, InputOutput, 8096> =
        Session::new(io, "tls13.akamai.io", &mut rng);

    tls.write("GET / HTTP/1.0\r\nHost: tls13.akamai.io\r\n\r\n".as_bytes())
        .unwrap();

    loop {
        let mut buf = [0u8; 512];
        match tls.read(&mut buf) {
            Ok(len) => {
                println!("{}", unsafe { core::str::from_utf8_unchecked(&buf[..len]) });
            }
            Err(err) => {
                println!("Got error: {:?}", err);
                break;
            }
        }
    }

    println!("That's it for now");

    loop {}
}

pub struct Rng {}

impl Rng {
    pub fn new() -> Rng {
        Rng {}
    }
}

impl rand_core::CryptoRng for Rng {}

impl RngCore for Rng {
    fn next_u32(&mut self) -> u32 {
        unsafe { (&*esp32c3_hal::pac::RNG::ptr()).data.read().bits() }
    }

    fn next_u64(&mut self) -> u64 {
        self.next_u32() as u64 | ((self.next_u32() as u64) << 32)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for byte in dest {
            *byte = (self.next_u32() & 0xff) as u8;
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

struct InputOutput<'a> {
    interface: esp_wifi::wifi_interface::Wifi<'a>,
    socket: SocketHandle,
    current_millis_fn: fn() -> u32,
}

impl<'a> InputOutput<'a> {
    pub fn new(
        interface: esp_wifi::wifi_interface::Wifi<'a>,
        socket: SocketHandle,
        current_millis_fn: fn() -> u32,
    ) -> InputOutput<'a> {
        InputOutput {
            interface,
            socket,
            current_millis_fn,
        }
    }
}

#[derive(Debug)]
enum IoError {
    Other(smoltcp::Error),
}

impl embedded_io::Error for IoError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl<'a> Io for InputOutput<'a> {
    type Error = IoError;
}

impl<'a> Read for InputOutput<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        loop {
            self.interface
                .network_interface()
                .poll(Instant::from_millis((self.current_millis_fn)()))
                .unwrap();

            let socket = self
                .interface
                .network_interface()
                .get_socket::<TcpSocket>(self.socket);

            if socket.may_recv() {
                break;
            }
        }

        loop {
            let res = self
                .interface
                .network_interface()
                .poll(Instant::from_millis((self.current_millis_fn)()));

            if let Ok(false) = res {
                break;
            }
        }

        loop {
            self.interface
                .network_interface()
                .poll(Instant::from_millis((self.current_millis_fn)()))
                .unwrap();

            let socket = self
                .interface
                .network_interface()
                .get_socket::<TcpSocket>(self.socket);

            let res = socket.recv_slice(buf).map_err(|e| IoError::Other(e));
            if *res.as_ref().unwrap() != 0 {
                break res;
            }
        }
    }
}

impl<'a> Write for InputOutput<'a> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        loop {
            self.interface
                .network_interface()
                .poll(Instant::from_millis((self.current_millis_fn)()))
                .unwrap();

            let socket = self
                .interface
                .network_interface()
                .get_socket::<TcpSocket>(self.socket);

            if socket.may_send() {
                break;
            }
        }

        loop {
            let res = self
                .interface
                .network_interface()
                .poll(Instant::from_millis((self.current_millis_fn)()));

            if let Ok(false) = res {
                break;
            }
        }

        let socket = self
            .interface
            .network_interface()
            .get_socket::<TcpSocket>(self.socket);

        let res = socket.send_slice(buf).map_err(|e| IoError::Other(e));
        res
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        loop {
            let res = self
                .interface
                .network_interface()
                .poll(Instant::from_millis((self.current_millis_fn)()));

            if let Ok(false) = res {
                break;
            }
        }

        Ok(())
    }
}

pub fn current_millis() -> u32 {
    (esp_wifi::timer::get_systimer_count() * 1000 / esp_wifi::timer::TICKS_PER_SECOND) as u32
}

pub fn init_logger() {
    unsafe {
        log::set_logger_racy(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Info);
    }
}

static LOGGER: SimpleLogger = SimpleLogger;
struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        println!("{} - {}", record.level(), record.args());
    }

    fn flush(&self) {}
}
