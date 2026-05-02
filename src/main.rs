use std::{
    collections::VecDeque,
    fs::OpenOptions,
    sync::{Arc, Mutex},
    time::Duration,
};

use kvm_ioctls::{IoEventAddress, VcpuExit};
use vm_superio::{Serial, Trigger};
use vmm_sys_util::{
    poll::{PollContext, PollEvents},
    terminal::Terminal,
};

use scratch_new::{
    boot::{self, BootArtifacts},
    blk::Blk,
    bus::MMIOBus,
    event::EventWrapper,
    net,
    serial::DummySerialEvent,
    virtio::VirtioDevice,
};

type HostSerial = Serial<EventWrapper, DummySerialEvent, std::io::Stdout>;

const BLK_MMIO_MEM_START: u64 = MMIO_MEM_START + 0x1000;

const COM1: u16 = 0x3f8;
const MMIO_MEM_START: u64 = 0xd0000000;
const BLOCK_PATH: &str = "/home/frsk/prj/kernel_images/rootfs.ext4";
const KERNEL_PATH: &str = "/home/frsk/prj/kernel_images/linux-6.6.99/vmlinux";
const QUEUE_NOTIFY_OFFSET: u64 = 0x50;
const GUEST_MAC: [u8; 6] = [0x02, 0x12, 0x34, 0x56, 0x78, 0x9a];

const NET_NUM_QUEUES: usize = 2;
const TAP_RX_TOKEN: u8 = 3 + NET_NUM_QUEUES as u8;
const BLK_QUEUE_TOKEN: u8 = TAP_RX_TOKEN + 1;

fn main() {
    let BootArtifacts {
        vm,
        vcpu,
        guest_mem,
    } = boot::setup(KERNEL_PATH);

    let mut mmio_bus = MMIOBus::new();

    let virtio_net = net::Net::new("tap0", Some(GUEST_MAC)).expect("failed to create virtio net");
    vm.register_irqfd(virtio_net.interrupt_event(), 5).unwrap();
    for (q_sel, evt) in virtio_net.queues_events().iter().enumerate() {
        let addr = IoEventAddress::Mmio(QUEUE_NOTIFY_OFFSET + MMIO_MEM_START);
        vm.register_ioevent(evt, &addr, q_sel as u32).unwrap();
    }

    let virtio_net_activate_evt = virtio_net.activate_event.try_clone().unwrap();
    let virtio_net_queue_events = virtio_net
        .queues_events()
        .iter()
        .map(|evt| evt.try_clone().unwrap())
        .collect::<Vec<_>>();
    let virtio_net = Arc::new(Mutex::new(virtio_net));
    mmio_bus
        .insert_virtio(MMIO_MEM_START, virtio_net.clone(), guest_mem.clone())
        .expect("insert virtio net mmio");

    let virtio_blk = Blk::new(
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(BLOCK_PATH)
            .expect("failed to open block path"),
    )
    .expect("failed to create virtio blk");
    vm.register_irqfd(virtio_blk.interrupt_event(), 6).unwrap();
    for (q_sel, evt) in virtio_blk.queues_events().iter().enumerate() {
        let addr = IoEventAddress::Mmio(QUEUE_NOTIFY_OFFSET + BLK_MMIO_MEM_START);
        vm.register_ioevent(evt, &addr, q_sel as u32).unwrap();
    }
    let virtio_blk_queue_events = virtio_blk
        .queues_events()
        .iter()
        .map(|evt| evt.try_clone().unwrap())
        .collect::<Vec<_>>();
    let virtio_blk = Arc::new(Mutex::new(virtio_blk));
    mmio_bus
        .insert_virtio(BLK_MMIO_MEM_START, virtio_blk.clone(), guest_mem.clone())
        .expect("insert virtio blk mmio");

    let com_evt_1 = EventWrapper::new();
    vm.register_irqfd(&com_evt_1.0, 4).unwrap();

    let stdio_serial = Arc::new(Mutex::new(Serial::with_events(
        com_evt_1.try_clone().unwrap(),
        DummySerialEvent,
        std::io::stdout(),
    )));

    let exit_evt = EventWrapper::new();
    spawn_vcpu_runtime(vcpu, stdio_serial.clone(), mmio_bus, exit_evt.try_clone().unwrap());

    let stdin = std::io::stdin().lock();
    stdin.set_raw_mode().expect("failed to set raw mode");

    let poll: PollContext<u8> = PollContext::new().unwrap();
    poll.add(&exit_evt.0, 0).unwrap();
    poll.add(&stdin, 1).unwrap();
    poll.add(&virtio_net_activate_evt, 2).unwrap();
    for (q_sel, evt) in virtio_net_queue_events.iter().enumerate() {
        poll.add(evt, 3 + q_sel as u8).unwrap();
    }
    for (q_sel, evt) in virtio_blk_queue_events.iter().enumerate() {
        poll.add(evt, BLK_QUEUE_TOKEN + q_sel as u8).unwrap();
    }

    let mut tap_rx_registered = false;
    let mut pending_stdin = VecDeque::new();

    'l: loop {
        flush_pending_stdin(&stdio_serial, &mut pending_stdin);

        let events: PollEvents<u8> = if pending_stdin.is_empty() {
            poll.wait().unwrap()
        } else {
            poll.wait_timeout(Duration::from_millis(10)).unwrap()
        };
        for event in events.iter_readable() {
            match event.token() {
                0 => {
                    println!("vcpu stopped, main loop exit");
                    break 'l;
                }
                1 => {
                    let mut out = [0u8; 64];
                    match stdin.read_raw(&mut out[..]) {
                        Ok(0) => {}
                        Ok(count) => {
                            pending_stdin.extend(&out[..count]);
                            flush_pending_stdin(&stdio_serial, &mut pending_stdin);
                        }
                        Err(e) => {
                            eprintln!("error while reading stdin: {:?}", e);
                        }
                    }
                }
                2 => {
                    let _ = virtio_net_activate_evt.read().unwrap();
                    if !tap_rx_registered {
                        virtio_net
                            .lock()
                            .unwrap()
                            .register_tap_readable(&poll, TAP_RX_TOKEN)
                            .unwrap();
                        tap_rx_registered = true;
                    }
                }
                TAP_RX_TOKEN => {
                    let mut net = virtio_net.lock().unwrap();
                    let _ = net.process_rx();
                }
                token if token >= 3 && token < 3 + NET_NUM_QUEUES as u8 => {
                    let idx = (token - 3) as usize;
                    let _ = virtio_net_queue_events[idx].read().unwrap();
                    let mut net = virtio_net.lock().unwrap();
                    let _ = net.process_tx();
                }
                token if token >= BLK_QUEUE_TOKEN
                    && token < BLK_QUEUE_TOKEN + virtio_blk_queue_events.len() as u8 =>
                {
                    let idx = (token - BLK_QUEUE_TOKEN) as usize;
                    let _ = virtio_blk_queue_events[idx].read().unwrap();
                    let mut blk = virtio_blk.lock().unwrap();
                    let _ = blk.process_queue();
                }
                _ => unreachable!(),
            }
        }
    }
}

fn spawn_vcpu_runtime(
    mut vcpu: kvm_ioctls::VcpuFd,
    stdio_serial: Arc<Mutex<HostSerial>>,
    mut mmio_bus: MMIOBus,
    vcpu_exit_evt: EventWrapper,
) {
    std::thread::spawn(move || {
        loop {
            match vcpu.run() {
                Ok(run) => match run {
                    VcpuExit::IoIn(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            data[0] = lock_host_serial(&stdio_serial).read((addr - COM1) as u8)
                        }
                    }
                    VcpuExit::IoOut(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            let _ = lock_host_serial(&stdio_serial).write((addr - COM1) as u8, data[0]);
                        }
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        mmio_bus.read(addr, data);
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        mmio_bus.write(addr, data);
                    }
                    VcpuExit::Hlt => {
                        println!("KVM_EXIT_HLT");
                        break;
                    }
                    VcpuExit::Shutdown => {
                        println!("KVM_EXIT_SHUTDOWN");
                        break;
                    }
                    r => {
                        println!("KVM_EXIT: {:?}", r)
                    }
                },
                Err(e) => {
                    println!("KVM Run error {:?}", e);
                    break;
                }
            }
        }
        vcpu_exit_evt.trigger().unwrap();
    });
}

fn lock_host_serial(
    serial: &Arc<Mutex<HostSerial>>,
) -> std::sync::MutexGuard<'_, HostSerial> {
    match serial.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn flush_pending_stdin(serial: &Arc<Mutex<HostSerial>>, pending: &mut VecDeque<u8>) {
    loop {
        if pending.is_empty() {
            break;
        }

        let written = {
            let contiguous = pending.make_contiguous();
            let mut serial = lock_host_serial(serial);
            match serial.enqueue_raw_bytes(contiguous) {
                Ok(written) => written,
                Err(vm_superio::serial::Error::FullFifo) => break,
                Err(e) => {
                    println!("failed to enqueue bytes: {:?}", e);
                    break;
                }
            }
        };

        if written == 0 {
            break;
        }

        pending.drain(..written);
    }
}
