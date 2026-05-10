use std::{
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vm_superio::Trigger;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use crate::virtio::{DescriptorChain, Queue, VirtioDevice};

const VIRTIO_TYPE_BLOCK: u32 = 2;
const SECTOR_SHIFT: u8 = 9;
const SECTOR_SIZE: u64 = 1 << SECTOR_SHIFT;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_F_FLUSH: u64 = 9;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const BLK_NUM_QUEUES: usize = 1;
const BLK_QUEUE_SIZE: u16 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestType {
    In,
    Out,
    Flush,
    Unsupported(u32),
}

fn test(  ) {
    println!("jsjdjsj");
}

impl From<u32> for RequestType {
    fn from(value: u32) -> Self {
        match value {
            VIRTIO_BLK_T_IN => Self::In,
            VIRTIO_BLK_T_OUT => Self::Out,
            VIRTIO_BLK_T_FLUSH => Self::Flush,
            _ => Self::Unsupported(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct RequestHeader {
    request_type: u32,
    reserved: u32,
    sector: u64,
}

unsafe impl ByteValued for RequestHeader {}

#[derive(Debug)]
struct Request {
    request_type: RequestType,
    data: Vec<(GuestAddress, u32)>,
    sector: u64,
    status_addr: GuestAddress,
}

impl Request {
    fn request_type(&self) -> RequestType {
        self.request_type
    }

    fn data(&self) -> &[(GuestAddress, u32)] {
        &self.data
    }

    fn sector(&self) -> u64 {
        self.sector
    }

    fn status_addr(&self) -> GuestAddress {
        self.status_addr
    }

    fn total_data_len(&self) -> u64 {
        self.data.iter().map(|(_, len)| u64::from(*len)).sum()
    }
}

fn copy_from_guest<M, W>(mem: &M, ranges: &[(GuestAddress, u32)], mut writer: W) -> io::Result<u64>
where
    M: GuestMemoryBackend,
    W: Write,
{
    let mut copied = 0;
    let mut buffer = vec![0; 4096];

    for &(addr, len) in ranges {
        let mut remaining = len as usize;
        let mut offset = 0u64;

        while remaining > 0 {
            let chunk_len = remaining.min(buffer.len());
            let chunk_addr = addr.unchecked_add(offset);
            mem.read_slice(&mut buffer[..chunk_len], chunk_addr)
                .map_err(|_| io::Error::other("failed to read guest memory"))?;
            writer.write_all(&buffer[..chunk_len])?;
            remaining -= chunk_len;
            offset += chunk_len as u64;
            copied += chunk_len as u64;
        }
    }

    Ok(copied)
}

#[derive(Debug)]
struct PartialIoError {
    completed: u64,
}

fn copy_to_guest<M, R>(
    mem: &M,
    ranges: &[(GuestAddress, u32)],
    mut reader: R,
) -> Result<u64, PartialIoError>
where
    M: GuestMemoryBackend,
    R: Read,
{
    let mut copied = 0;
    let mut buffer = vec![0; 4096];

    for &(addr, len) in ranges {
        let mut remaining = len as usize;
        let mut offset = 0u64;

        while remaining > 0 {
            let chunk_len = remaining.min(buffer.len());
            if reader.read_exact(&mut buffer[..chunk_len]).is_err() {
                return Err(PartialIoError { completed: copied });
            }
            let chunk_addr = addr.unchecked_add(offset);
            mem.write_slice(&buffer[..chunk_len], chunk_addr)
                .map_err(|_| PartialIoError { completed: copied })?;
            remaining -= chunk_len;
            offset += chunk_len as u64;
            copied += chunk_len as u64;
        }
    }

    Ok(copied)
}

#[derive(Debug)]
enum ParseError {
    DescriptorChainTooShort,
    DescriptorLengthTooSmall,
    GuestMemory,
    InvalidFlushSector,
    UnexpectedReadOnlyDescriptor,
    UnexpectedWriteOnlyDescriptor,
}

#[derive(Debug)]
enum ExecuteError {
    InvalidAccess,
    InvalidDataLength,
    Read(u32),
    Seek(io::Error),
    Unsupported(u32),
    Write(io::Error),
    Flush(io::Error),
}

impl ExecuteError {
    fn status(&self) -> u8 {
        match self {
            Self::Unsupported(_) => VIRTIO_BLK_S_UNSUPP,
            _ => VIRTIO_BLK_S_IOERR,
        }
    }
}

impl fmt::Display for ExecuteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAccess => f.write_str("invalid access"),
            Self::InvalidDataLength => f.write_str("invalid data length"),
            Self::Read(partial) => write!(f, "read error after {partial} bytes"),
            Self::Seek(err) => write!(f, "seek error: {err}"),
            Self::Unsupported(value) => write!(f, "unsupported request type {value}"),
            Self::Write(err) => write!(f, "write error: {err}"),
            Self::Flush(err) => write!(f, "flush error: {err}"),
        }
    }
}

impl std::error::Error for ExecuteError {}

#[derive(Debug)]
enum CompleteError {
    GuestMemory,
    Overflow,
}

impl Request {
    fn parse<M>(head: DescriptorChain<M>) -> Result<Self, ParseError>
    where
        M: Clone + Deref,
        M::Target: GuestMemoryBackend,
    {
        if head.is_write_only() {
            return Err(ParseError::UnexpectedWriteOnlyDescriptor);
        }

        let request_header = head
            .mem
            .read_obj::<RequestHeader>(GuestAddress(head.addr))
            .map_err(|_| ParseError::GuestMemory)?;

        let request_type = RequestType::from(request_header.request_type);
        if request_type == RequestType::Flush && request_header.sector != 0 {
            return Err(ParseError::InvalidFlushSector);
        }

        let mut desc = head.next().ok_or(ParseError::DescriptorChainTooShort)?;
        let mut data = Vec::new();

        while desc.next().is_some() {
            if request_type == RequestType::In && !desc.is_write_only() {
                return Err(ParseError::UnexpectedReadOnlyDescriptor);
            }

            if request_type == RequestType::Out && desc.is_write_only() {
                return Err(ParseError::UnexpectedWriteOnlyDescriptor);
            }

            data.push((GuestAddress(desc.addr), desc.len));
            desc = desc.next().ok_or(ParseError::DescriptorChainTooShort)?;
        }

        if !desc.is_write_only() {
            return Err(ParseError::UnexpectedReadOnlyDescriptor);
        }

        if desc.len < 1 {
            return Err(ParseError::DescriptorLengthTooSmall);
        }

        Ok(Self {
            request_type,
            data,
            sector: request_header.sector,
            status_addr: GuestAddress(desc.addr),
        })
    }
}

#[derive(Clone, Copy)]
enum IrqType {
    Config,
    Vring,
}

struct IrqTrigger {
    irq_evt: EventFd,
    irq_status: Arc<AtomicU32>,
}

impl Clone for IrqTrigger {
    fn clone(&self) -> Self {
        Self {
            irq_evt: self.irq_evt.try_clone().unwrap(),
            irq_status: Arc::clone(&self.irq_status),
        }
    }
}

impl IrqTrigger {
    fn new(irq_status: Arc<AtomicU32>) -> io::Result<Self> {
        Ok(Self {
            irq_evt: EventFd::new(EFD_NONBLOCK)?,
            irq_status,
        })
    }

    fn trigger(&self, irq_type: IrqType) -> io::Result<()> {
        let bit = match irq_type {
            IrqType::Config => 0x2,
            IrqType::Vring => 0x1,
        };
        self.irq_status.fetch_or(bit, Ordering::SeqCst);
        self.irq_evt.write(1)
    }
}

impl Trigger for IrqTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.trigger(IrqType::Config)
    }
}

#[derive(Default, Clone)]
struct ConfigSpace(Vec<u8>);

impl ConfigSpace {
    fn new(capacity_sectors: u64) -> Self {
        let mut bytes = vec![0; 8];
        bytes[..8].copy_from_slice(&capacity_sectors.to_le_bytes());
        Self(bytes)
    }

    fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }
}

enum DeviceState {
    Inactive,
    Activated { mem: GuestMemoryMmap },
}

impl Default for DeviceState {
    fn default() -> Self {
        Self::Inactive
    }
}

impl DeviceState {
    fn is_activated(&self) -> bool {
        matches!(self, Self::Activated { .. })
    }

    fn unwrap(&self) -> GuestMemoryMmap {
        match self {
            Self::Activated { mem } => mem.clone(),
            Self::Inactive => panic!("device not activated"),
        }
    }
}

pub struct Blk {
    disk_image: File,
    disk_size: u64,
    capacity_sectors: u64,
    avail_features: u64,
    acked_features: u64,
    queues: [Queue; BLK_NUM_QUEUES],
    queue_events: [EventFd; BLK_NUM_QUEUES],
    irq_trigger: IrqTrigger,
    interrupt_status: Arc<AtomicU32>,
    config_space: ConfigSpace,
    state: DeviceState,
}

impl Blk {
    pub fn new(disk_image: File) -> io::Result<Self> {
        let disk_size = disk_image.metadata()?.len();
        let capacity_sectors = disk_size / SECTOR_SIZE;
        let interrupt_status = Arc::new(AtomicU32::new(0));

        Ok(Self {
            disk_image,
            disk_size,
            capacity_sectors,
            avail_features: 1 << VIRTIO_BLK_F_FLUSH,
            acked_features: 0,
            queues: [Queue::new(BLK_QUEUE_SIZE)],
            queue_events: [EventFd::new(EFD_NONBLOCK)?],
            irq_trigger: IrqTrigger::new(Arc::clone(&interrupt_status))?,
            interrupt_status,
            config_space: ConfigSpace::new(capacity_sectors),
            state: DeviceState::Inactive,
        })
    }

    pub fn interrupt_event(&self) -> &EventFd {
        &self.irq_trigger.irq_evt
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    pub fn disk_size(&self) -> u64 {
        self.disk_size
    }

    pub fn process_queue(&mut self) -> io::Result<()> {
        let mem = self.state.unwrap();
        let mut try_notify = false;

        loop {
            let desc = match self.queues[0].pop(&mem) {
                Ok(Some(desc)) => desc,
                Ok(None) => break,
                Err(_) => break,
            };

            let desc_index = desc.index;
            let request = match Request::parse(desc) {
                Ok(request) => request,
                Err(_) => {
                    self.queues[0].push(&mem, desc_index, 0);
                    try_notify = true;
                    continue;
                }
            };

            let result = self.execute_request(&mem, &request);
            let complete_result =
                Self::complete_request(&mem, &mut self.queues[0], desc_index, &request, result);
            if complete_result.is_ok() {
                try_notify = true;
            }
        }

        let should_notify = {
            let queue = &mut self.queues[0];
            try_notify && queue.prepare_notify(&mem)
        };
        if should_notify {
            self.irq_trigger.trigger(IrqType::Vring)?;
        }
        Ok(())
    }

    fn check_access(&self, sector: u64, sectors_count: u64) -> Result<(), ExecuteError> {
        let end_sector = sector
            .checked_add(sectors_count)
            .ok_or(ExecuteError::InvalidAccess)?;
        if end_sector > self.capacity_sectors {
            return Err(ExecuteError::InvalidAccess);
        }
        Ok(())
    }

    fn execute_request<M>(&mut self, mem: &M, request: &Request) -> Result<u32, ExecuteError>
    where
        M: GuestMemoryBackend,
    {
        let request_type = request.request_type();
        let total_len = request.total_data_len();

        if matches!(request_type, RequestType::In | RequestType::Out)
            && total_len % SECTOR_SIZE != 0
        {
            return Err(ExecuteError::InvalidDataLength);
        }

        let offset = request
            .sector()
            .checked_shl(u32::from(SECTOR_SHIFT))
            .ok_or(ExecuteError::InvalidAccess)?;

        match request_type {
            RequestType::In => {
                self.check_access(request.sector(), total_len / SECTOR_SIZE)?;
                self.disk_image
                    .seek(SeekFrom::Start(offset))
                    .map_err(ExecuteError::Seek)?;
                copy_to_guest(mem, request.data(), &mut self.disk_image)
                    .map_err(|e| ExecuteError::Read(e.completed as u32))?;
                Ok(total_len as u32)
            }
            RequestType::Out => {
                self.check_access(request.sector(), total_len / SECTOR_SIZE)?;
                self.disk_image
                    .seek(SeekFrom::Start(offset))
                    .map_err(ExecuteError::Seek)?;
                copy_from_guest(mem, request.data(), &mut self.disk_image).map_err(|_| {
                    ExecuteError::Write(io::Error::other("failed to write disk image"))
                })?;
                Ok(0)
            }
            RequestType::Flush => self
                .disk_image
                .sync_all()
                .map(|_| 0)
                .map_err(ExecuteError::Flush),
            RequestType::Unsupported(value) => Err(ExecuteError::Unsupported(value)),
        }
    }

    fn complete_request<M>(
        mem: &M,
        queue: &mut Queue,
        desc_index: u16,
        request: &Request,
        result: Result<u32, ExecuteError>,
    ) -> Result<(), CompleteError>
    where
        M: GuestMemoryBackend,
    {
        let (status, used_len) = match result {
            Ok(len) => {
                let used_len = len.checked_add(1).ok_or(CompleteError::Overflow)?;
                (VIRTIO_BLK_S_OK, used_len)
            }
            Err(err) => {
                let used_len = match err {
                    ExecuteError::Read(partial) => {
                        partial.checked_add(1).ok_or(CompleteError::Overflow)?
                    }
                    _ => 1,
                };
                (err.status(), used_len)
            }
        };

        if mem.write_obj(status, request.status_addr()).is_err() {
            return Err(CompleteError::GuestMemory);
        }

        queue.push(mem, desc_index, used_len);

        Ok(())
    }
}

impl VirtioDevice for Blk {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        VIRTIO_TYPE_BLOCK
    }

    fn queues(&self) -> &[Queue] {
        self.queues.as_slice()
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        self.queues.as_mut_slice()
    }

    fn queues_events(&self) -> &[EventFd] {
        self.queue_events.as_slice()
    }

    fn interrupt_trigger(&self) -> &dyn Trigger<E = io::Error> {
        &self.irq_trigger
    }

    fn interrupt_status(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.interrupt_status)
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) {
        if let Some(config_space_bytes) = self.config_space.as_slice().get(offset as usize..) {
            let len = config_space_bytes.len().min(data.len());
            data[..len].copy_from_slice(&config_space_bytes[..len]);
        }
    }

    fn write_config(&mut self, _offset: u32, _data: &[u8]) {}

    fn activate(&mut self, mem: GuestMemoryMmap) -> io::Result<()> {
        self.state = DeviceState::Activated { mem };
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.state.is_activated()
    }
}
