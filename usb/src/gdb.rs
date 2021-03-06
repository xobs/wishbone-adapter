extern crate byteorder;
use std::io;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use super::bridge::{Bridge, BridgeError};
use super::riscv::{RiscvCpu, RiscvCpuError};
use super::Config;

use crate::gdb::byteorder::ByteOrder;
use byteorder::{BigEndian, NativeEndian};

pub struct GdbServer {
    connection: TcpStream,
    no_ack_mode: bool,
    is_alive: bool,
    last_signal: u8,
}

#[derive(Debug)]
pub enum GdbServerError {
    /// Rust standard IO error
    IoError(io::Error),

    /// The network connection has closed
    ConnectionClosed,

    /// We were unable to parse an integer
    ParseIntError,

    /// Something happened with the CPU
    CpuError(RiscvCpuError),

    /// The bridge failed somehow
    BridgeError(BridgeError),
}

impl std::convert::From<BridgeError> for GdbServerError {
    fn from(e: BridgeError) -> Self {
        GdbServerError::BridgeError(e)
    }
}

impl std::convert::From<RiscvCpuError> for GdbServerError {
    fn from(e: RiscvCpuError) -> Self {
        GdbServerError::CpuError(e)
    }
}

impl std::convert::From<io::Error> for GdbServerError {
    fn from(e: io::Error) -> Self {
        GdbServerError::IoError(e)
    }
}

impl std::convert::From<std::num::ParseIntError> for GdbServerError {
    fn from(_e: std::num::ParseIntError) -> Self {
        GdbServerError::ParseIntError
    }
}

#[derive(Debug)]
enum BreakPointType {
    BreakSoft,
    BreakHard,
    WatchWrite,
    WatchRead,
    WatchAccess,
}

impl BreakPointType {
    fn from_str(r: &str) -> Result<BreakPointType, GdbServerError> {
        match r {
            "0" => Ok(BreakPointType::BreakSoft),
            "1" => Ok(BreakPointType::BreakHard),
            "2" => Ok(BreakPointType::WatchWrite),
            "3" => Ok(BreakPointType::WatchRead),
            "4" => Ok(BreakPointType::WatchAccess),
            _ => Err(GdbServerError::ParseIntError),
        }
    }
}

#[derive(Debug)]
enum GdbCommand {
    Unknown(String),

    /// qSupported
    SupportedQueries(String),

    /// QStartNoAckMode
    StartNoAckMode,

    /// Hg#
    SetCurrentThread(u64),

    /// Hc# (# may be -1)
    ContinueThread(i32),

    /// ?
    LastSignalPacket,

    /// qfThreadInfo
    GetThreadInfo,

    /// qC
    GetCurrentThreadId,

    /// qAttached
    CheckIsAttached,

    /// g
    GetRegisters,

    /// p#
    GetRegister(u32),

    /// qSymbol::
    SymbolsReady,

    /// m#,#
    ReadMemory(u32 /* addr */, u32 /* length */),

    /// vCont?
    VContQuery,

    /// vCont;c
    VContContinue,

    /// vCont;C04:0;c
    VContContinueFromSignal(String),

    /// vCont;s:0;c
    VContStepFromSignal(String),

    /// c
    Continue,

    /// s
    Step,

    /// Ctrl-C
    Interrupt,

    /// qRcmd,
    MonitorCommand(String),

    /// Z0,###,2
    AddBreakpoint(
        BreakPointType,
        u32, /* address */
        u32, /* length */
    ),

    /// z0,###,2
    RemoveBreakpoint(
        BreakPointType,
        u32, /* address */
        u32, /* length */
    ),

    /// qOffsets
    GetOffsets,

    /// qXfer:features:read:target.xml:0,1000
    ReadFeature(
        String, /* filename */
        u32,    /* offset */
        u32,    /* len */
    ),

    /// qXfer:threads:read::0,1000
    ReadThreads(u32 /* offset */, u32 /* len */),
}

impl GdbServer {
    pub fn new(cfg: &Config) -> Result<GdbServer, GdbServerError> {
        let listener = TcpListener::bind(format!("{}:{}", cfg.bind_addr, cfg.bind_port))?;

        // accept connections and process them serially
        println!(
            "Accepting connections on {}:{}",
            cfg.bind_addr, cfg.bind_port
        );
        let (connection, _sockaddr) = listener.accept()?;
        println!("Connection from {:?}", connection.peer_addr()?);
        Ok(GdbServer {
            connection,
            no_ack_mode: false,
            is_alive: true,
            last_signal: 0,
        })
    }

    fn packet_to_command(&self, pkt: &[u8]) -> Result<GdbCommand, GdbServerError> {
        let pkt = String::from_utf8_lossy(pkt).to_string();

        if pkt == "qSupported" || pkt.starts_with("qSupported:") {
            Ok(GdbCommand::SupportedQueries(pkt))
        } else if pkt == "QStartNoAckMode" {
            Ok(GdbCommand::StartNoAckMode)
        } else if pkt == "qAttached" {
            Ok(GdbCommand::CheckIsAttached)
        } else if pkt == "qOffsets" {
            Ok(GdbCommand::GetOffsets)
        } else if pkt.starts_with("qXfer:features:read:") {
            let pkt = pkt.trim_start_matches("qXfer:features:read:");
            let fields: Vec<&str> = pkt.split(':').collect();
            let offsets: Vec<&str> = fields[1].split(',').collect();
            let offset = u32::from_str_radix(offsets[0], 16)?;
            let len = u32::from_str_radix(offsets[1], 16)?;
            Ok(GdbCommand::ReadFeature(fields[0].to_string(), offset, len))
        } else if pkt.starts_with("qXfer:threads:read::") {
            let pkt = pkt.trim_start_matches("qXfer:threads:read::");
            let offsets: Vec<&str> = pkt.split(',').collect();
            let offset = u32::from_str_radix(offsets[0], 16)?;
            let len = u32::from_str_radix(offsets[1], 16)?;
            Ok(GdbCommand::ReadThreads(offset, len))
        } else if pkt.starts_with("Z") {
            let pkt = pkt.trim_start_matches("Z");
            let fields: Vec<&str> = pkt.split(',').collect();
            let bptype = BreakPointType::from_str(fields[0])?;
            let address = u32::from_str_radix(fields[1], 16)?;
            let size = u32::from_str_radix(fields[2], 16)?;
            Ok(GdbCommand::AddBreakpoint(bptype, address, size))
        } else if pkt.starts_with("z") {
            let pkt = pkt.trim_start_matches("z");
            let fields: Vec<&str> = pkt.split(',').collect();
            let bptype = BreakPointType::from_str(fields[0])?;
            let address = u32::from_str_radix(fields[1], 16)?;
            let size = u32::from_str_radix(fields[2], 16)?;
            Ok(GdbCommand::RemoveBreakpoint(bptype, address, size))
        } else if pkt.starts_with("qRcmd,") {
            let pkt = pkt.trim_start_matches("qRcmd,");
            let pkt_bytes = pkt.as_bytes();
            let mut tmp1 = Vec::new();
            let mut acc = 0;
            for i in 0..pkt.len() {
                let nybble = if pkt_bytes[i] >= 0x30 && pkt_bytes[i] <= 0x39 {
                    pkt_bytes[i] - 0x30
                } else if pkt_bytes[i] >= 0x61 && pkt_bytes[i] <= 0x66 {
                    pkt_bytes[i] + 10 - 0x61
                } else if pkt_bytes[i] >= 0x41 && pkt_bytes[i] <= 0x46 {
                    pkt_bytes[i] + 10 - 0x41
                } else {
                    0
                };
                if i & 1 == 1 {
                    tmp1.push((acc << 4) | nybble);
                    acc = 0;
                } else {
                    acc = nybble;
                }
            }
            Ok(GdbCommand::MonitorCommand(
                String::from_utf8_lossy(&tmp1).to_string(),
            ))
        } else if pkt == "g" {
            Ok(GdbCommand::GetRegisters)
        } else if pkt == "c" {
            Ok(GdbCommand::Continue)
        } else if pkt == "s" {
            Ok(GdbCommand::Step)
        } else if pkt.starts_with("m") {
            let pkt = pkt.trim_start_matches("m").to_string();
            let v: Vec<&str> = pkt.split(',').collect();
            let addr = u32::from_str_radix(v[0], 16)?;
            let length = u32::from_str_radix(v[1], 16)?;
            Ok(GdbCommand::ReadMemory(addr, length))
        } else if pkt.starts_with("p") {
            Ok(GdbCommand::GetRegister(u32::from_str_radix(
                pkt.trim_start_matches("r"),
                16,
            )?))
        } else if pkt.starts_with("Hg") {
            Ok(GdbCommand::SetCurrentThread(u64::from_str_radix(
                pkt.trim_start_matches("Hg"),
                16,
            )?))
        } else if pkt.starts_with("Hc") {
            Ok(GdbCommand::ContinueThread(i32::from_str_radix(
                pkt.trim_start_matches("Hc"),
                16,
            )?))
        } else if pkt == "qC" {
            Ok(GdbCommand::GetCurrentThreadId)
        } else if pkt == "?" {
            Ok(GdbCommand::LastSignalPacket)
        } else if pkt == "qfThreadInfo" {
            Ok(GdbCommand::GetThreadInfo)
        } else if pkt == "vCont?" {
            Ok(GdbCommand::VContQuery)
        } else if pkt == "vCont;c" {
            Ok(GdbCommand::VContContinue)
        } else if pkt.starts_with("vCont;C") {
            //vCont;C04:0;c
            let pkt = pkt.trim_start_matches("vCont;C").to_string();
            let v: Vec<&str> = pkt.split(',').collect();
            Ok(GdbCommand::VContContinueFromSignal(pkt))
        } else if pkt.starts_with("vCont;s") {
            let pkt = pkt.trim_start_matches("vCont;s").to_string();
            Ok(GdbCommand::VContStepFromSignal(pkt))
        } else if pkt == "qSymbol::" {
            Ok(GdbCommand::SymbolsReady)
        } else {
            Ok(GdbCommand::Unknown(pkt))
        }
    }

    fn get_command(&mut self) -> Result<GdbCommand, GdbServerError> {
        let mut buffer = [0; 16384];
        let mut byte = [0; 1];
        let mut remote_checksum = [0; 2];
        let mut buffer_offset = 0;

        // XXX Replace this with a BufReader for performance
        loop {
            let len = self.connection.read(&mut byte)?;
            if len == 0 {
                return Err(GdbServerError::ConnectionClosed);
            }

            match byte[0] {
                0x24 /*'$'*/ => {
                    let mut checksum: u8 = 0;
                    loop {
                        let len = self.connection.read(&mut byte)?;
                        if len == 0 {
                            return Err(GdbServerError::ConnectionClosed);
                        }
                        match byte[0] as char {
                            '#' => {
                                // There's got to be a better way to compare the checksum
                                self.connection.read(&mut remote_checksum)?;
                                let checksum_str = format!("{:02x}", checksum);
                                if checksum_str != String::from_utf8_lossy(&remote_checksum) {
                                    println!(
                                        "Checksum mismatch: Calculated {:?} vs {}",
                                        checksum_str,
                                        String::from_utf8_lossy(&remote_checksum)
                                    );
                                    self.gdb_send_nak()?;
                                } else {
                                    if !self.no_ack_mode {
                                        self.gdb_send_ack()?;
                                    }
                                }
                                let (buffer, _remainder) = buffer.split_at(buffer_offset);
                                println!("<- Read packet ${:?}#{:#?}", String::from_utf8_lossy(buffer), String::from_utf8_lossy(&remote_checksum));
                                return self.packet_to_command(&buffer);
                            }
                            other => {
                                buffer[buffer_offset] = other as u8;
                                buffer_offset = buffer_offset + 1;
                                checksum = checksum.wrapping_add(other as u8);
                            }
                        }
                    }
                }
                0x2b /*'+'*/ => {}
                0x2d /*'-'*/ => {}
                0x3 => return Ok(GdbCommand::Interrupt),
                other => println!("Warning: unrecognied byte received: {}", other),
            }
        }
    }

    pub fn process(&mut self, cpu: &RiscvCpu, bridge: &Bridge) -> Result<(), GdbServerError> {
        let cmd = self.get_command()?;

        println!("<- Read packet {:?}", cmd);
        match cmd {
            GdbCommand::SupportedQueries(_) => self.gdb_send(b"PacketSize=3fff;qXfer:memory-map:read+;qXfer:features:read+;qXfer:threads:read+;QStartNoAckMode+;vContSupported+")?,
            GdbCommand::StartNoAckMode => { self.no_ack_mode = true; self.gdb_send(b"OK")?},
            GdbCommand::SetCurrentThread(_) => self.gdb_send(b"OK")?,
            GdbCommand::ContinueThread(_) => self.gdb_send(b"OK")?,
            GdbCommand::AddBreakpoint(_, _, _) => self.gdb_send(b"OK")?,
            GdbCommand::RemoveBreakpoint(_, _, _) => self.gdb_send(b"OK")?,
            GdbCommand::LastSignalPacket => {
                let sig_str = format!("S{:02x}", self.last_signal);
                self.gdb_send(if self.is_alive { sig_str.as_bytes() } else { b"W00" })?
            },
            GdbCommand::GetThreadInfo => self.gdb_send(b"l")?,
            GdbCommand::GetCurrentThreadId => self.gdb_send(b"QC0")?,
            GdbCommand::CheckIsAttached => self.gdb_send(b"1")?,
            GdbCommand::GetRegisters => {
                let mut register_list = String::new();
                for i in 0..33 {
                    register_list.push_str(format!("{:08x}", i).as_str());
                }
                self.gdb_send(register_list.as_bytes())?
            }
            GdbCommand::GetRegister(_) => self.gdb_send(b"12345678")?,
            GdbCommand::SymbolsReady => self.gdb_send(b"OK")?,
            GdbCommand::ReadMemory(addr, len) => {
                let mut values = vec![];
                for offset in (0 .. len).step_by(4) {
                    values.push(cpu.read_memory(bridge, addr + offset, 4)?);
                }
                self.gdb_send_u32(values)?
            },
            GdbCommand::VContQuery => self.gdb_send(b"vCont;c;C;s;S")?,
            GdbCommand::VContContinue => cpu.resume(bridge)?,
            GdbCommand::VContContinueFromSignal(_) => cpu.resume(bridge)?,
            GdbCommand::VContStepFromSignal(_) => {
                cpu.step(bridge)?;
                self.gdb_send(format!("S{:02x}", self.last_signal).as_bytes())?;
            },
            GdbCommand::GetOffsets => self.gdb_send(b"Text=0;Data=0;Bss=0")?,
            GdbCommand::Continue => cpu.resume(&bridge)?,
            GdbCommand::Step => cpu.step(&bridge)?,
            GdbCommand::MonitorCommand(_) => self.gdb_send(b"OK")?,
            GdbCommand::ReadFeature(filename, offset, len) => {
                self.gdb_send_file(cpu.get_feature(&filename)?, offset, len)?
            },
            GdbCommand::ReadThreads(offset, len) => self.gdb_send_file(cpu.get_threads()?, offset, len)?,
            GdbCommand::Interrupt => {
                self.last_signal = 2;
                cpu.halt(bridge)?;
                self.gdb_send(format!("S{:02x}", self.last_signal).as_bytes())?
            },
            GdbCommand::Unknown(_) => self.gdb_send(b"")?,
        };
        Ok(())
    }

    fn gdb_send_ack(&mut self) -> io::Result<usize> {
        self.connection.write(&['+' as u8])
    }

    fn gdb_send_nak(&mut self) -> io::Result<usize> {
        self.connection.write(&['-' as u8])
    }

    fn gdb_send_u32(&mut self, vals: Vec<u32>) -> io::Result<()> {
        let mut out_str = String::new();
        for val in vals {
            let mut buf = [0; 4];
            BigEndian::write_u32(&mut buf, val);
            out_str.push_str(&format!("{:08x}", NativeEndian::read_u32(&buf)));
        }
        self.gdb_send(out_str.as_bytes())
    }

    fn gdb_send(&mut self, inp: &[u8]) -> io::Result<()> {
        let mut buffer = [0; 16388];
        let mut checksum: u8 = 0;
        buffer[0] = '$' as u8;
        for i in 0..inp.len() {
            buffer[i + 1] = inp[i];
            checksum = checksum.wrapping_add(inp[i]);
        }
        let checksum_str = &format!("{:02x}", checksum);
        let checksum_bytes = checksum_str.as_bytes();
        buffer[inp.len() + 1] = '#' as u8;
        buffer[inp.len() + 2] = checksum_bytes[0];
        buffer[inp.len() + 3] = checksum_bytes[1];
        let (to_write, _rest) = buffer.split_at(inp.len() + 4);
        println!(
            "-> Writing {} bytes: {}",
            to_write.len(),
            String::from_utf8_lossy(&to_write)
        );
        self.connection.write(&to_write)?;
        Ok(())
    }

    fn gdb_send_file(&mut self, mut data: Vec<u8>, offset: u32, len: u32) -> io::Result<()> {
        let offset = offset as usize;
        let len = len as usize;
        let mut end = offset + len;
        if offset > data.len() {
            self.gdb_send(b"l")?;
        } else {
            if end > data.len() {
                end = data.len();
            }
            let mut trimmed_features: Vec<u8> = data.drain(offset..end).collect();
            if trimmed_features.len() >= len {
                // XXX should this be <= or < ?
                trimmed_features.insert(0, 'm' as u8);
            } else {
                trimmed_features.insert(0, 'l' as u8);
            }
            self.gdb_send(&trimmed_features)?;
        }
        Ok(())
    }
}
